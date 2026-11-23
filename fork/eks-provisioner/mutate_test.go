// SPDX-License-Identifier: CC0-1.0

package main

import (
	"encoding/json"
	"errors"
	"testing"

	jsonpatch "gopkg.in/evanphx/json-patch.v4"
	admissionv1 "k8s.io/api/admission/v1"
	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
)

var testInjection = endpointInjection{EndpointURL: "https://172.29.0.1:9597", ConfigMapName: "aws-emulator-ca-0123456789"}

// patched is pod after the injection's JSON patch, or pod itself when the injection
// leaves it alone.
func patched(t *testing.T, namespace string, pod corev1.Pod) corev1.Pod {
	t.Helper()
	operations := testInjection.patch(namespace, &pod)
	if operations == nil {
		return pod
	}
	return applyPatch(t, pod, operations)
}

func applyPatch(t *testing.T, pod corev1.Pod, operations any) corev1.Pod {
	t.Helper()
	document, err := json.Marshal(pod)
	if err != nil {
		t.Fatal(err)
	}
	encoded, err := json.Marshal(operations)
	if err != nil {
		t.Fatal(err)
	}
	patch, err := jsonpatch.DecodePatch(encoded)
	if err != nil {
		t.Fatalf("the patch is not a JSON patch: %v\n%s", err, encoded)
	}
	result, err := patch.Apply(document)
	if err != nil {
		t.Fatalf("the patch does not apply: %v\n%s", err, encoded)
	}
	var out corev1.Pod
	if err := json.Unmarshal(result, &out); err != nil {
		t.Fatal(err)
	}
	return out
}

func envOf(container corev1.Container) map[string][]string {
	values := map[string][]string{}
	for _, variable := range container.Env {
		values[variable.Name] = append(values[variable.Name], variable.Value)
	}
	return values
}

func appPod() corev1.Pod {
	return corev1.Pod{
		Spec: corev1.PodSpec{
			InitContainers: []corev1.Container{{Name: "migrate", Image: "app"}},
			Containers: []corev1.Container{
				{Name: "api", Image: "app", Env: []corev1.EnvVar{{Name: "LOG_LEVEL", Value: "info"}}},
				{Name: "sidecar", Image: "proxy", VolumeMounts: []corev1.VolumeMount{{Name: "config", MountPath: "/etc/proxy"}}},
			},
			Volumes: []corev1.Volume{{Name: "config", VolumeSource: corev1.VolumeSource{EmptyDir: &corev1.EmptyDirVolumeSource{}}}},
		},
	}
}

func TestEveryContainerAndInitContainerGetsTheEmulatorsEndpointAndCA(t *testing.T) {
	pod := patched(t, "loopzero-system", appPod())

	containers := append(append([]corev1.Container{}, pod.Spec.InitContainers...), pod.Spec.Containers...)
	if len(containers) != 3 {
		t.Fatalf("the patch changed the containers: %+v", containers)
	}
	for _, container := range containers {
		env := envOf(container)
		for name, want := range map[string]string{
			"AWS_ENDPOINT_URL":     "https://172.29.0.1:9597",
			"AWS_ENDPOINT_URL_STS": "https://172.29.0.1:9597",
			"AWS_CA_BUNDLE":        "/etc/aws-emulator/ca-bundle.pem",
			"SSL_CERT_FILE":        "/etc/aws-emulator/ca-bundle.pem",
			"NODE_EXTRA_CA_CERTS":  "/etc/aws-emulator/ca.pem",
		} {
			if got := env[name]; len(got) != 1 || got[0] != want {
				t.Errorf("container %s: %s is %v, want [%s]", container.Name, name, got, want)
			}
		}
		mounted := false
		for _, mount := range container.VolumeMounts {
			if mount.Name == "aws-emulator-ca" && mount.MountPath == "/etc/aws-emulator" && mount.ReadOnly {
				mounted = true
			}
		}
		if !mounted {
			t.Errorf("container %s does not mount the CA read-only at /etc/aws-emulator: %+v", container.Name, container.VolumeMounts)
		}
	}
	if got := envOf(pod.Spec.Containers[0])["LOG_LEVEL"]; len(got) != 1 || got[0] != "info" {
		t.Errorf("the container's own variable was lost: %v", got)
	}
	if len(pod.Spec.Containers[1].VolumeMounts) != 2 || pod.Spec.Containers[1].VolumeMounts[0].Name != "config" {
		t.Errorf("the container's own mount was lost: %+v", pod.Spec.Containers[1].VolumeMounts)
	}
	var volume *corev1.Volume
	for index := range pod.Spec.Volumes {
		if pod.Spec.Volumes[index].Name == "aws-emulator-ca" {
			volume = &pod.Spec.Volumes[index]
		}
	}
	if volume == nil || volume.ConfigMap == nil || volume.ConfigMap.Name != "aws-emulator-ca-0123456789" {
		t.Fatalf("the pod has no volume of the CA ConfigMap: %+v", pod.Spec.Volumes)
	}
	if len(pod.Spec.Volumes) != 2 {
		t.Errorf("the pod's own volume was lost: %+v", pod.Spec.Volumes)
	}
}

func TestAPodWithNoVolumesAndNoEnvGetsThemCreated(t *testing.T) {
	pod := patched(t, "default", corev1.Pod{Spec: corev1.PodSpec{Containers: []corev1.Container{{Name: "aws", Image: "aws-cli"}}}})

	if envOf(pod.Spec.Containers[0])["AWS_ENDPOINT_URL"] == nil || len(pod.Spec.Volumes) != 1 || len(pod.Spec.Containers[0].VolumeMounts) != 1 {
		t.Fatalf("got %+v", pod.Spec)
	}
}

func TestAVariableAContainerAlreadySetsIsKept(t *testing.T) {
	pod := appPod()
	pod.Spec.Containers[0].Env = append(pod.Spec.Containers[0].Env, corev1.EnvVar{Name: "SSL_CERT_FILE", Value: "/etc/ssl/mine.pem"})

	got := envOf(patched(t, "default", pod).Spec.Containers[0])["SSL_CERT_FILE"]

	if len(got) != 1 || got[0] != "/etc/ssl/mine.pem" {
		t.Fatalf("SSL_CERT_FILE is %v, want the container's own value only", got)
	}
}

func TestAPodThatAlreadySetsTheEndpointIsLeftAlone(t *testing.T) {
	pod := appPod()
	pod.Spec.Containers[1].Env = []corev1.EnvVar{{Name: "AWS_ENDPOINT_URL", Value: "https://localstack:4566"}}

	if operations := testInjection.patch("default", &pod); operations != nil {
		t.Fatalf("a pod that names its own endpoint was patched: %+v", operations)
	}
}

func TestAPodWhoseInitContainerSetsTheEndpointIsLeftAlone(t *testing.T) {
	pod := appPod()
	pod.Spec.InitContainers[0].Env = []corev1.EnvVar{{Name: "AWS_ENDPOINT_URL", Value: "https://localstack:4566"}}

	if operations := testInjection.patch("default", &pod); operations != nil {
		t.Fatalf("a pod that names its own endpoint was patched: %+v", operations)
	}
}

func TestPodsInKubeSystemAreLeftAlone(t *testing.T) {
	pod := appPod()

	if operations := testInjection.patch("kube-system", &pod); operations != nil {
		t.Fatalf("a kube-system pod was patched: %+v", operations)
	}
}

func TestAKubeSystemPodWithAnIRSARoleGetsTheEmulatorsEndpoint(t *testing.T) {
	// The AWS Load Balancer Controller runs in kube-system with an IRSA role: without the
	// endpoint it exchanges its token at the real STS.
	pod := appPod()
	pod.Spec.Containers[0].Env = append(pod.Spec.Containers[0].Env, corev1.EnvVar{Name: "AWS_ROLE_ARN", Value: "arn:aws:iam::000000000000:role/lb-controller"})

	got := patched(t, "kube-system", pod)

	if envOf(got.Spec.Containers[0])["AWS_ENDPOINT_URL"] == nil {
		t.Fatalf("a kube-system pod with an IRSA role got no endpoint: %+v", got.Spec.Containers[0].Env)
	}
}

func TestAKubeSystemPodWithTheIRSATokenVolumeGetsTheEmulatorsEndpoint(t *testing.T) {
	pod := appPod()
	pod.Spec.Volumes = append(pod.Spec.Volumes, corev1.Volume{Name: "aws-iam-token"})

	got := patched(t, "kube-system", pod)

	if envOf(got.Spec.Containers[1])["AWS_ENDPOINT_URL"] == nil {
		t.Fatalf("a kube-system pod with the IRSA token volume got no endpoint")
	}
}

func TestAPodPatchedOnceIsNotPatchedAgain(t *testing.T) {
	pod := patched(t, "default", appPod())

	if operations := testInjection.patch("default", &pod); operations != nil {
		t.Fatalf("a patched pod was patched again: %+v", operations)
	}
}

func review(t *testing.T, namespace string, pod corev1.Pod, dryRun bool) admissionv1.AdmissionReview {
	t.Helper()
	raw, err := json.Marshal(pod)
	if err != nil {
		t.Fatal(err)
	}
	return admissionv1.AdmissionReview{Request: &admissionv1.AdmissionRequest{
		UID:       types.UID("7c2a"),
		Namespace: namespace,
		Operation: admissionv1.Create,
		Resource:  podsResource,
		Object:    runtime.RawExtension{Raw: raw},
		DryRun:    &dryRun,
	}}
}

func TestTheAdmissionResponseCarriesTheJSONPatchAndEnsuresTheCAInTheNamespace(t *testing.T) {
	var ensured []string
	response := admit(review(t, "loopzero-system", appPod(), false), testInjection, func(namespace string) error {
		ensured = append(ensured, namespace)
		return nil
	})

	if !response.Allowed || response.UID != "7c2a" || response.PatchType == nil || *response.PatchType != admissionv1.PatchTypeJSONPatch {
		t.Fatalf("got %+v", response)
	}
	var operations []map[string]any
	if err := json.Unmarshal(response.Patch, &operations); err != nil {
		t.Fatalf("the patch is not JSON: %v", err)
	}
	pod := applyPatch(t, appPod(), operations)
	if envOf(pod.Spec.Containers[0])["AWS_ENDPOINT_URL"] == nil {
		t.Fatalf("the response's patch does not set the endpoint: %s", response.Patch)
	}
	if len(ensured) != 1 || ensured[0] != "loopzero-system" {
		t.Fatalf("the CA ConfigMap was ensured in %v", ensured)
	}
}

func TestADryRunAdmissionCreatesNoConfigMap(t *testing.T) {
	response := admit(review(t, "default", appPod(), true), testInjection, func(string) error {
		t.Fatal("a dry run created the CA ConfigMap")
		return nil
	})

	if !response.Allowed || response.Patch == nil {
		t.Fatalf("got %+v", response)
	}
}

func TestAPodWhoseCACannotBeProvidedIsRefused(t *testing.T) {
	response := admit(review(t, "default", appPod(), false), testInjection, func(string) error { return errors.New("forbidden") })

	if response.Allowed || response.Result == nil || response.Result.Message == "" {
		t.Fatalf("got %+v", response)
	}
}

func TestAPodLeftAloneIsAdmittedUnchangedWithoutTouchingTheNamespace(t *testing.T) {
	response := admit(review(t, "kube-system", appPod(), false), testInjection, func(string) error {
		t.Fatal("a kube-system pod ensured the CA ConfigMap")
		return nil
	})

	if !response.Allowed || response.Patch != nil {
		t.Fatalf("got %+v", response)
	}
}
