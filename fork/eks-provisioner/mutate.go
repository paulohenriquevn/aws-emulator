// SPDX-License-Identifier: CC0-1.0

package main

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"strconv"
	"strings"

	admissionv1 "k8s.io/api/admission/v1"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

const (
	// caVolumeName is the volume the emulator's CA reaches a pod's containers through.
	caVolumeName = "aws-emulator-ca"
	// caMountPath is where every container finds the emulator's CA.
	caMountPath = "/etc/aws-emulator"
)

// excludedNamespaces are left as Amazon EKS leaves them: the cluster's own components
// run there and keep starting while the provisioner, which serves this webhook, is down.
// A pod there that has an IRSA role (the AWS Load Balancer Controller, the EBS CSI
// driver) still gets the endpoint, or it would exchange its token at the real STS.
var excludedNamespaces = map[string]bool{"kube-system": true}

// irsaTokenVolume is the volume amazon-eks-pod-identity-webhook adds for the role's token.
const irsaTokenVolume = "aws-iam-token"

// hasIRSARole is whether the pod identity webhook gave pod a role.
func hasIRSARole(pod *corev1.Pod) bool {
	for _, volume := range pod.Spec.Volumes {
		if volume.Name == irsaTokenVolume {
			return true
		}
	}
	for _, container := range append(append([]corev1.Container{}, pod.Spec.InitContainers...), pod.Spec.Containers...) {
		if setsVariable(container, "AWS_ROLE_ARN") {
			return true
		}
	}
	return false
}

var podsResource = metav1.GroupVersionResource{Version: "v1", Resource: "pods"}

// endpointInjection is what makes a pod's AWS clients reach the emulator instead of
// *.amazonaws.com: its endpoint, and its CA from the ConfigMap ensureTrustBundle creates.
type endpointInjection struct {
	EndpointURL   string
	ConfigMapName string
}

// patchOperation is one RFC 6902 JSON patch operation.
type patchOperation struct {
	Op    string `json:"op"`
	Path  string `json:"path"`
	Value any    `json:"value"`
}

// environment is what every container gets, unless it sets the variable itself.
// AWS_ENDPOINT_URL_STS is there because the AWS SDK for Go v2 builds the STS client of
// its default credential chain before it reads AWS_ENDPOINT_URL. SSL_CERT_FILE serves
// clients that ignore AWS_CA_BUNDLE (Go's and OpenSSL's own HTTP clients), and points at
// a bundle that keeps the public roots, so a pod still reaches GHCR or any other public
// endpoint. NODE_EXTRA_CA_CERTS adds the CA for Node.js, which reads neither.
func (injection endpointInjection) environment() []corev1.EnvVar {
	bundle := caMountPath + "/" + combinedBundleKey
	return []corev1.EnvVar{
		{Name: "AWS_ENDPOINT_URL", Value: injection.EndpointURL},
		{Name: "AWS_ENDPOINT_URL_STS", Value: injection.EndpointURL},
		{Name: "AWS_CA_BUNDLE", Value: bundle},
		{Name: "SSL_CERT_FILE", Value: bundle},
		{Name: "NODE_EXTRA_CA_CERTS", Value: caMountPath + "/" + emulatorCAKey},
	}
}

// patch is the JSON patch that gives every container and init container of pod the
// emulator's endpoint and CA, or nil when the pod is left alone: it runs in an excluded
// namespace with no IRSA role, already names an endpoint of its own, or was patched already.
func (injection endpointInjection) patch(namespace string, pod *corev1.Pod) []patchOperation {
	if excludedNamespaces[namespace] && !hasIRSARole(pod) {
		return nil
	}
	for _, volume := range pod.Spec.Volumes {
		if volume.Name == caVolumeName {
			return nil
		}
	}
	for _, container := range append(append([]corev1.Container{}, pod.Spec.InitContainers...), pod.Spec.Containers...) {
		for _, variable := range container.Env {
			if variable.Name == "AWS_ENDPOINT_URL" {
				return nil
			}
		}
	}
	volume := corev1.Volume{Name: caVolumeName, VolumeSource: corev1.VolumeSource{ConfigMap: &corev1.ConfigMapVolumeSource{
		LocalObjectReference: corev1.LocalObjectReference{Name: injection.ConfigMapName},
	}}}
	operations := appendTo(nil, "/spec/volumes", len(pod.Spec.Volumes) == 0, volume)
	mount := corev1.VolumeMount{Name: caVolumeName, MountPath: caMountPath, ReadOnly: true}
	fields := []struct {
		name       string
		containers []corev1.Container
	}{{"initContainers", pod.Spec.InitContainers}, {"containers", pod.Spec.Containers}}
	for _, field := range fields {
		for index, container := range field.containers {
			base := "/spec/" + field.name + "/" + strconv.Itoa(index)
			var missing []any
			for _, variable := range injection.environment() {
				if !setsVariable(container, variable.Name) {
					missing = append(missing, variable)
				}
			}
			operations = appendTo(operations, base+"/env", len(container.Env) == 0, missing...)
			operations = appendTo(operations, base+"/volumeMounts", len(container.VolumeMounts) == 0, mount)
		}
	}
	return operations
}

// appendTo adds values to the array at path, creating it when the object has none.
func appendTo(operations []patchOperation, path string, absent bool, values ...any) []patchOperation {
	if len(values) == 0 {
		return operations
	}
	if absent {
		return append(operations, patchOperation{Op: "add", Path: path, Value: values})
	}
	for _, value := range values {
		operations = append(operations, patchOperation{Op: "add", Path: path + "/-", Value: value})
	}
	return operations
}

func setsVariable(container corev1.Container, name string) bool {
	for _, variable := range container.Env {
		if variable.Name == name {
			return true
		}
	}
	return false
}

// admit answers one AdmissionReview for a pod. A pod that gets the injection has the CA
// ConfigMap ensured in its namespace first (not on a dry run, which must have no side
// effect), and is refused when that fails, since it would never start without it.
func admit(review admissionv1.AdmissionReview, injection endpointInjection, ensureCA func(namespace string) error) *admissionv1.AdmissionResponse {
	request := review.Request
	response := &admissionv1.AdmissionResponse{UID: request.UID, Allowed: true}
	if request.Resource != podsResource || request.Operation != admissionv1.Create {
		return response
	}
	var pod corev1.Pod
	if err := json.Unmarshal(request.Object.Raw, &pod); err != nil {
		return refuse(response, "the object is not a pod: "+err.Error())
	}
	operations := injection.patch(request.Namespace, &pod)
	if operations == nil {
		return response
	}
	if request.DryRun == nil || !*request.DryRun {
		if err := ensureCA(request.Namespace); err != nil {
			return refuse(response, fmt.Sprintf("the emulator's CA could not be provided in namespace %s: %v", request.Namespace, err))
		}
	}
	patch, err := json.Marshal(operations)
	if err != nil {
		return refuse(response, "encoding the patch: "+err.Error())
	}
	patchType := admissionv1.PatchTypeJSONPatch
	response.Patch, response.PatchType = patch, &patchType
	return response
}

func refuse(response *admissionv1.AdmissionResponse, message string) *admissionv1.AdmissionResponse {
	response.Allowed = false
	response.Result = &metav1.Status{Status: metav1.StatusFailure, Message: message, Reason: metav1.StatusReasonInternalError, Code: http.StatusInternalServerError}
	return response
}

// servePodMutations answers POST /mutate/{region}/{cluster}, the endpoint injection
// webhook the cluster's API server calls for every pod it creates.
func (p *provisioner) servePodMutations(writer http.ResponseWriter, request *http.Request) {
	region, cluster, ok := strings.Cut(strings.TrimPrefix(request.URL.Path, "/mutate/"), "/")
	if request.Method != http.MethodPost || !ok || region == "" || cluster == "" {
		http.Error(writer, "POST /mutate/{region}/{cluster}", http.StatusNotFound)
		return
	}
	var review admissionv1.AdmissionReview
	if err := json.NewDecoder(http.MaxBytesReader(writer, request.Body, 4<<20)).Decode(&review); err != nil || review.Request == nil {
		http.Error(writer, fmt.Sprintf("the body is not an AdmissionReview with a request: %v", err), http.StatusBadRequest)
		return
	}
	injection := endpointInjection{EndpointURL: p.podEndpoint, ConfigMapName: p.trust.configMapName()}
	review.Response = admit(review, injection, func(namespace string) error {
		return p.ensureTrustBundle(request.Context(), region, cluster, namespace)
	})
	if !review.Response.Allowed {
		p.log.Error("pod refused", "cluster", cluster, "region", region, "namespace", review.Request.Namespace, "reason", review.Response.Result.Message)
	}
	review.Request = nil
	writer.Header().Set("Content-Type", "application/json")
	if err := json.NewEncoder(writer).Encode(review); err != nil {
		p.log.Error("answering an AdmissionReview", "error", err)
	}
}

func (p *provisioner) ensureTrustBundle(ctx context.Context, region, cluster, namespace string) error {
	clientset, err := p.clientset(region, cluster)
	if err != nil {
		return err
	}
	return ensureTrustBundle(ctx, clientset, p.trust, namespace)
}
