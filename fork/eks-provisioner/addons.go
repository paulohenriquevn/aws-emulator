// SPDX-License-Identifier: CC0-1.0

package main

import (
	"context"
	"crypto/x509"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"net/netip"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	ekstypes "github.com/aws/aws-sdk-go-v2/service/eks/types"
	admissionregistrationv1 "k8s.io/api/admissionregistration/v1"
	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	rbacv1 "k8s.io/api/rbac/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/apimachinery/pkg/util/intstr"
	"k8s.io/client-go/kubernetes"
)

// What Amazon EKS runs on a cluster's control plane besides the API server, and this
// provisioner installs in each kind cluster: amazon-eks-pod-identity-webhook (IRSA), and
// the webhook that points the cluster's pods at the emulator.
const (
	// podIdentityWebhookImage is the webhook's v0.6.19 release, the image its README
	// publishes on Amazon ECR Public, pinned by the digest of its multi-arch index.
	podIdentityWebhookImage = "public.ecr.aws/eks/amazon-eks-pod-identity-webhook:v0.6.19@sha256:160ec19c2222016fa28db5e5269b233368ac788ed2d76479a2730cabff3cf693"
	podIdentityWebhookName  = "pod-identity-webhook"
	podIdentityNamespace    = "kube-system"
	podIdentityCertSecret   = "pod-identity-webhook-cert"
	endpointWebhookName     = "aws-emulator-endpoint"

	managedByLabel = "app.kubernetes.io/managed-by"
	managedBy      = "aws-emulator-eks-provisioner"
	fieldManager   = "aws-emulator-eks-provisioner"

	controlPlaneRole = "node-role.kubernetes.io/control-plane"

	// addonsInterval is how often a running cluster's add-ons are applied again, which
	// undoes a change made to them by hand.
	addonsInterval = time.Minute
)

func objectMeta(name, namespace string) metav1.ObjectMeta {
	return metav1.ObjectMeta{Name: name, Namespace: namespace, Labels: map[string]string{managedByLabel: managedBy}}
}

// podIdentityWebhookDeployment runs the webhook on the control-plane node, where Amazon
// EKS runs it: it serves pods created before any node group has joined, and takes no
// capacity from the nodes.
func podIdentityWebhookDeployment(region string) *appsv1.Deployment {
	labels := map[string]string{"app": podIdentityWebhookName}
	replicas := int32(1)
	return &appsv1.Deployment{
		TypeMeta:   metav1.TypeMeta{APIVersion: "apps/v1", Kind: "Deployment"},
		ObjectMeta: objectMeta(podIdentityWebhookName, podIdentityNamespace),
		Spec: appsv1.DeploymentSpec{
			Replicas: &replicas,
			Selector: &metav1.LabelSelector{MatchLabels: labels},
			Template: corev1.PodTemplateSpec{
				ObjectMeta: metav1.ObjectMeta{Labels: labels},
				Spec: corev1.PodSpec{
					ServiceAccountName: podIdentityWebhookName,
					PriorityClassName:  "system-cluster-critical",
					NodeSelector:       map[string]string{controlPlaneRole: ""},
					Tolerations:        []corev1.Toleration{{Key: controlPlaneRole, Operator: corev1.TolerationOpExists, Effect: corev1.TaintEffectNoSchedule}},
					Containers: []corev1.Container{{
						Name:  podIdentityWebhookName,
						Image: podIdentityWebhookImage,
						Command: []string{
							"/webhook",
							// The webhook reads its certificate from files, and reloads it.
							"--in-cluster=false",
							"--namespace=" + podIdentityNamespace,
							"--service-name=" + podIdentityWebhookName,
							"--port=8443",
							"--tls-cert=/etc/webhook/certs/tls.crt",
							"--tls-key=/etc/webhook/certs/tls.key",
							"--annotation-prefix=eks.amazonaws.com",
							"--token-audience=sts.amazonaws.com",
							"--aws-default-region=" + region,
							"--sts-regional-endpoint=true",
						},
						Ports:        []corev1.ContainerPort{{ContainerPort: 8443, Name: "https"}},
						VolumeMounts: []corev1.VolumeMount{{Name: "cert", MountPath: "/etc/webhook/certs", ReadOnly: true}},
					}},
					Volumes: []corev1.Volume{{Name: "cert", VolumeSource: corev1.VolumeSource{Secret: &corev1.SecretVolumeSource{SecretName: podIdentityCertSecret}}}},
				},
			},
		},
	}
}

// podIdentityWebhookConfiguration calls the webhook as Amazon EKS does: for every pod
// created, ignoring the webhook when it is down, and skipping a pod that opts out.
func podIdentityWebhookConfiguration(caPEM []byte) *admissionregistrationv1.MutatingWebhookConfiguration {
	ignore := admissionregistrationv1.Ignore
	none := admissionregistrationv1.SideEffectClassNone
	path := "/mutate"
	return &admissionregistrationv1.MutatingWebhookConfiguration{
		TypeMeta:   metav1.TypeMeta{APIVersion: "admissionregistration.k8s.io/v1", Kind: "MutatingWebhookConfiguration"},
		ObjectMeta: objectMeta(podIdentityWebhookName, ""),
		Webhooks: []admissionregistrationv1.MutatingWebhook{{
			Name: "pod-identity-webhook.amazonaws.com",
			ClientConfig: admissionregistrationv1.WebhookClientConfig{
				Service:  &admissionregistrationv1.ServiceReference{Name: podIdentityWebhookName, Namespace: podIdentityNamespace, Path: &path},
				CABundle: caPEM,
			},
			FailurePolicy: &ignore,
			ObjectSelector: &metav1.LabelSelector{MatchExpressions: []metav1.LabelSelectorRequirement{
				{Key: "eks.amazonaws.com/skip-pod-identity-webhook", Operator: metav1.LabelSelectorOpDoesNotExist},
			}},
			Rules:                   []admissionregistrationv1.RuleWithOperations{podCreation()},
			SideEffects:             &none,
			AdmissionReviewVersions: []string{"v1beta1"},
		}},
	}
}

// endpointWebhookConfiguration calls the provisioner at url for every pod created outside
// kube-system. A pod admitted while the provisioner is down would call *.amazonaws.com
// with nothing to say so, so the API server refuses it instead (failurePolicy Fail) and
// its controller creates it again; kube-system is excluded so the cluster's own
// components keep starting meanwhile. A second webhook takes kube-system with
// failurePolicy Ignore, for its pods with an IRSA role, and is called again after the pod
// identity webhook gives a pod its role (reinvocationPolicy IfNeeded). The webhook creates
// the CA ConfigMap a pod mounts, which it does not do on a dry run (NoneOnDryRun).
func endpointWebhookConfiguration(url string, caPEM []byte) *admissionregistrationv1.MutatingWebhookConfiguration {
	fail := admissionregistrationv1.Fail
	sideEffects := admissionregistrationv1.SideEffectClassNoneOnDryRun
	timeout := int32(10)
	excluded := make([]string, 0, len(excludedNamespaces))
	for namespace := range excludedNamespaces {
		excluded = append(excluded, namespace)
	}
	ignore := admissionregistrationv1.Ignore
	reinvoke := admissionregistrationv1.IfNeededReinvocationPolicy
	return &admissionregistrationv1.MutatingWebhookConfiguration{
		TypeMeta:   metav1.TypeMeta{APIVersion: "admissionregistration.k8s.io/v1", Kind: "MutatingWebhookConfiguration"},
		ObjectMeta: objectMeta(endpointWebhookName, ""),
		Webhooks: []admissionregistrationv1.MutatingWebhook{{
			Name:          "endpoint.eks-provisioner.aws-emulator.internal",
			ClientConfig:  admissionregistrationv1.WebhookClientConfig{URL: &url, CABundle: caPEM},
			FailurePolicy: &fail,
			NamespaceSelector: &metav1.LabelSelector{MatchExpressions: []metav1.LabelSelectorRequirement{
				{Key: "kubernetes.io/metadata.name", Operator: metav1.LabelSelectorOpNotIn, Values: excluded},
			}},
			Rules:                   []admissionregistrationv1.RuleWithOperations{podCreation()},
			SideEffects:             &sideEffects,
			TimeoutSeconds:          &timeout,
			AdmissionReviewVersions: []string{"v1"},
		}, {
			Name:               "system-endpoint.eks-provisioner.aws-emulator.internal",
			ClientConfig:       admissionregistrationv1.WebhookClientConfig{URL: &url, CABundle: caPEM},
			FailurePolicy:      &ignore,
			ReinvocationPolicy: &reinvoke,
			NamespaceSelector: &metav1.LabelSelector{MatchExpressions: []metav1.LabelSelectorRequirement{
				{Key: "kubernetes.io/metadata.name", Operator: metav1.LabelSelectorOpIn, Values: excluded},
			}},
			Rules:                   []admissionregistrationv1.RuleWithOperations{podCreation()},
			SideEffects:             &sideEffects,
			TimeoutSeconds:          &timeout,
			AdmissionReviewVersions: []string{"v1"},
		}},
	}
}

func podCreation() admissionregistrationv1.RuleWithOperations {
	return admissionregistrationv1.RuleWithOperations{
		Operations: []admissionregistrationv1.OperationType{admissionregistrationv1.Create},
		Rule:       admissionregistrationv1.Rule{APIGroups: []string{""}, APIVersions: []string{"v1"}, Resources: []string{"pods"}},
	}
}

// addonObjects is every object the add-ons are, in the order they are applied: the
// webhook's own objects before the configuration that sends pods to it.
func addonObjects(region, endpointURL string, webhookCA, servingCert, servingKey []byte) []runtime.Object {
	labels := map[string]string{"app": podIdentityWebhookName}
	return []runtime.Object{
		&corev1.ServiceAccount{TypeMeta: metav1.TypeMeta{APIVersion: "v1", Kind: "ServiceAccount"}, ObjectMeta: objectMeta(podIdentityWebhookName, podIdentityNamespace)},
		&rbacv1.ClusterRole{
			TypeMeta:   metav1.TypeMeta{APIVersion: "rbac.authorization.k8s.io/v1", Kind: "ClusterRole"},
			ObjectMeta: objectMeta(podIdentityWebhookName, ""),
			Rules:      []rbacv1.PolicyRule{{APIGroups: []string{""}, Resources: []string{"serviceaccounts"}, Verbs: []string{"get", "watch", "list"}}},
		},
		&rbacv1.ClusterRoleBinding{
			TypeMeta:   metav1.TypeMeta{APIVersion: "rbac.authorization.k8s.io/v1", Kind: "ClusterRoleBinding"},
			ObjectMeta: objectMeta(podIdentityWebhookName, ""),
			RoleRef:    rbacv1.RoleRef{APIGroup: "rbac.authorization.k8s.io", Kind: "ClusterRole", Name: podIdentityWebhookName},
			Subjects:   []rbacv1.Subject{{Kind: "ServiceAccount", Name: podIdentityWebhookName, Namespace: podIdentityNamespace}},
		},
		&corev1.Secret{
			TypeMeta:   metav1.TypeMeta{APIVersion: "v1", Kind: "Secret"},
			ObjectMeta: objectMeta(podIdentityCertSecret, podIdentityNamespace),
			Type:       corev1.SecretTypeTLS,
			Data:       map[string][]byte{corev1.TLSCertKey: servingCert, corev1.TLSPrivateKeyKey: servingKey},
		},
		podIdentityWebhookDeployment(region),
		&corev1.Service{
			TypeMeta:   metav1.TypeMeta{APIVersion: "v1", Kind: "Service"},
			ObjectMeta: objectMeta(podIdentityWebhookName, podIdentityNamespace),
			Spec: corev1.ServiceSpec{
				Selector: labels,
				Ports:    []corev1.ServicePort{{Port: 443, TargetPort: intstr.FromInt32(8443), Name: "https"}},
			},
		},
		podIdentityWebhookConfiguration(webhookCA),
		endpointWebhookConfiguration(endpointURL, webhookCA),
	}
}

// apply server-side applies object, so a second apply of the same object changes nothing
// and an object changed by hand is put back.
func apply(ctx context.Context, clientset kubernetes.Interface, object runtime.Object) error {
	data, err := json.Marshal(object)
	if err != nil {
		return err
	}
	force := true
	options := metav1.PatchOptions{FieldManager: fieldManager, Force: &force}
	patch := types.ApplyPatchType
	switch typed := object.(type) {
	case *corev1.ServiceAccount:
		_, err = clientset.CoreV1().ServiceAccounts(typed.Namespace).Patch(ctx, typed.Name, patch, data, options)
	case *corev1.Secret:
		_, err = clientset.CoreV1().Secrets(typed.Namespace).Patch(ctx, typed.Name, patch, data, options)
	case *corev1.Service:
		_, err = clientset.CoreV1().Services(typed.Namespace).Patch(ctx, typed.Name, patch, data, options)
	case *appsv1.Deployment:
		_, err = clientset.AppsV1().Deployments(typed.Namespace).Patch(ctx, typed.Name, patch, data, options)
	case *rbacv1.ClusterRole:
		_, err = clientset.RbacV1().ClusterRoles().Patch(ctx, typed.Name, patch, data, options)
	case *rbacv1.ClusterRoleBinding:
		_, err = clientset.RbacV1().ClusterRoleBindings().Patch(ctx, typed.Name, patch, data, options)
	case *admissionregistrationv1.MutatingWebhookConfiguration:
		_, err = clientset.AdmissionregistrationV1().MutatingWebhookConfigurations().Patch(ctx, typed.Name, patch, data, options)
	default:
		return fmt.Errorf("applying a %T is not implemented", object)
	}
	if err != nil {
		return fmt.Errorf("applying %s: %w", object.GetObjectKind().GroupVersionKind().Kind, err)
	}
	return nil
}

// installAddons applies the add-ons to a cluster whose VPC's gateway is gateway.
func (p *provisioner) installAddons(ctx context.Context, region, name string, gateway netip.Addr) error {
	cert, key, err := p.podIdentityServingCert(region, name)
	if err != nil {
		return err
	}
	clientset, err := p.clientset(region, name)
	if err != nil {
		return err
	}
	url := fmt.Sprintf("https://%s:%s/mutate/%s/%s", gateway, p.listenPort, region, name)
	for _, object := range addonObjects(region, url, p.webhook.caPEM, cert, key) {
		if err := apply(ctx, clientset, object); err != nil {
			return fmt.Errorf("the add-ons of %s: %w", name, err)
		}
	}
	p.addonsAt[region+"/"+name] = time.Now()
	return nil
}

// reconcileAddons applies a running cluster's add-ons again once addonsInterval passed
// since they last were.
func (p *provisioner) reconcileAddons(ctx context.Context, region string, described *ekstypes.Cluster) error {
	name := aws.ToString(described.Name)
	if time.Since(p.addonsAt[region+"/"+name]) < addonsInterval {
		return nil
	}
	cidr, _, err := p.vpcLayout(ctx, region, aws.ToString(described.ResourcesVpcConfig.VpcId))
	if err != nil {
		return err
	}
	return p.installAddons(ctx, region, name, gatewayOf(cidr))
}

// waitForPodIdentityWebhook waits until the webhook serves, so no pod is created without
// IRSA once the cluster is ACTIVE.
func (p *provisioner) waitForPodIdentityWebhook(ctx context.Context, region, name string) error {
	clientset, err := p.clientset(region, name)
	if err != nil {
		return err
	}
	return waitFor(ctx, 3*time.Minute, "the pod identity webhook of "+name+" becoming available", func() error {
		deployment, err := clientset.AppsV1().Deployments(podIdentityNamespace).Get(ctx, podIdentityWebhookName, metav1.GetOptions{})
		if err != nil {
			return err
		}
		if deployment.Status.ObservedGeneration < deployment.Generation || deployment.Status.AvailableReplicas < 1 {
			return fmt.Errorf("%d of %d replicas available; %s", deployment.Status.AvailableReplicas, *deployment.Spec.Replicas,
				podsState(ctx, clientset, podIdentityNamespace, "app="+podIdentityWebhookName))
		}
		return nil
	})
}

// podIdentityServingCert is the pod identity webhook's certificate for its Service,
// signed by the provisioner's webhook CA. It is kept in the cluster's state directory
// and issued again when missing, near its end, or signed by another CA.
func (p *provisioner) podIdentityServingCert(region, name string) (certPEM, keyPEM []byte, err error) {
	dir := filepath.Join(p.clusterDir(region, name), "pod-identity-webhook")
	certPath, keyPath := filepath.Join(dir, "tls.crt"), filepath.Join(dir, "tls.key")
	certPEM, certErr := os.ReadFile(certPath)
	keyPEM, keyErr := os.ReadFile(keyPath)
	if certErr == nil && keyErr == nil && p.webhook.stillValid(certPEM) {
		return certPEM, keyPEM, nil
	}
	service := podIdentityWebhookName + "." + podIdentityNamespace
	certPEM, keyPEM, err = p.webhook.issue(service+".svc", []string{service, service + ".svc", service + ".svc.cluster.local"}, nil)
	if err != nil {
		return nil, nil, fmt.Errorf("issuing the pod identity webhook's certificate: %w", err)
	}
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return nil, nil, err
	}
	if err := os.WriteFile(keyPath, keyPEM, 0o600); err != nil {
		return nil, nil, err
	}
	if err := os.WriteFile(certPath, certPEM, 0o644); err != nil {
		return nil, nil, err
	}
	return certPEM, keyPEM, nil
}

// stillValid is whether certPEM was signed by this CA and has more than 30 days left.
func (w *webhook) stillValid(certPEM []byte) bool {
	block, _ := pem.Decode(certPEM)
	if block == nil {
		return false
	}
	cert, err := x509.ParseCertificate(block.Bytes)
	if err != nil {
		return false
	}
	return cert.CheckSignatureFrom(w.ca) == nil && time.Now().AddDate(0, 0, 30).Before(cert.NotAfter)
}

// podsState says what the pods selector names are waiting for: their phase, why one is not
// scheduled, and what each container waits on (an image pull, a crash). It is what a
// deployment that never becomes available is reported with.
func podsState(ctx context.Context, clientset kubernetes.Interface, namespace, selector string) string {
	pods, err := clientset.CoreV1().Pods(namespace).List(ctx, metav1.ListOptions{LabelSelector: selector})
	if err != nil {
		return "its pods could not be listed: " + err.Error()
	}
	if len(pods.Items) == 0 {
		return "no pod matches " + selector
	}
	var states []string
	for _, pod := range pods.Items {
		state := fmt.Sprintf("pod %s is %s", pod.Name, pod.Status.Phase)
		for _, condition := range pod.Status.Conditions {
			if condition.Type == corev1.PodScheduled && condition.Status == corev1.ConditionFalse {
				state += fmt.Sprintf(", not scheduled (%s: %s)", condition.Reason, condition.Message)
			}
		}
		for _, container := range append(append([]corev1.ContainerStatus{}, pod.Status.InitContainerStatuses...), pod.Status.ContainerStatuses...) {
			if waiting := container.State.Waiting; waiting != nil {
				state += fmt.Sprintf(", container %s waiting (%s: %s)", container.Name, waiting.Reason, waiting.Message)
			}
			if terminated := container.LastTerminationState.Terminated; terminated != nil {
				state += fmt.Sprintf(", container %s last exited %d (%s)", container.Name, terminated.ExitCode, terminated.Reason)
			}
		}
		states = append(states, state)
	}
	return strings.Join(states, "; ")
}
