// SPDX-License-Identifier: CC0-1.0

package main

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"testing"

	admissionregistrationv1 "k8s.io/api/admissionregistration/v1"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/kubernetes/fake"
)

func TestThePodIdentityWebhookImageIsPinnedByVersionAndDigest(t *testing.T) {
	if !regexp.MustCompile(`^public\.ecr\.aws/eks/amazon-eks-pod-identity-webhook:v\d+\.\d+\.\d+@sha256:[0-9a-f]{64}$`).MatchString(podIdentityWebhookImage) {
		t.Fatalf("got %q", podIdentityWebhookImage)
	}
}

func TestThePodIdentityWebhookRunsOnTheControlPlane(t *testing.T) {
	spec := podIdentityWebhookDeployment("us-east-1").Spec.Template.Spec

	if _, ok := spec.NodeSelector["node-role.kubernetes.io/control-plane"]; !ok {
		t.Errorf("the webhook is not held to the control plane: %v", spec.NodeSelector)
	}
	tolerated := false
	for _, toleration := range spec.Tolerations {
		if toleration.Key == "node-role.kubernetes.io/control-plane" && toleration.Effect == corev1.TaintEffectNoSchedule {
			tolerated = true
		}
	}
	if !tolerated {
		t.Errorf("the webhook does not tolerate the control plane's taint: %+v", spec.Tolerations)
	}
}

func TestThePodIdentityWebhookInjectsTheSTSAudienceAndTheRegion(t *testing.T) {
	command := strings.Join(podIdentityWebhookDeployment("us-east-1").Spec.Template.Spec.Containers[0].Command, " ")

	for _, flag := range []string{"--in-cluster=false", "--token-audience=sts.amazonaws.com", "--aws-default-region=us-east-1", "--sts-regional-endpoint=true", "--annotation-prefix=eks.amazonaws.com"} {
		if !strings.Contains(command, flag) {
			t.Errorf("the webhook runs without %s: %s", flag, command)
		}
	}
}

func TestThePodIdentityWebhookIsCalledAsOnEKS(t *testing.T) {
	webhook := podIdentityWebhookConfiguration([]byte("ca")).Webhooks[0]

	if webhook.FailurePolicy == nil || *webhook.FailurePolicy != admissionregistrationv1.Ignore {
		t.Errorf("failurePolicy is %v, want Ignore", webhook.FailurePolicy)
	}
	if webhook.ClientConfig.Service == nil || webhook.ClientConfig.Service.Name != "pod-identity-webhook" || webhook.ClientConfig.Service.Namespace != "kube-system" {
		t.Errorf("clientConfig is %+v", webhook.ClientConfig)
	}
	if !bytes.Equal(webhook.ClientConfig.CABundle, []byte("ca")) {
		t.Errorf("the webhook is not trusted by its CA")
	}
}

func TestTheEndpointWebhookIsTheProvisionersAndRefusesPodsWhenItIsDown(t *testing.T) {
	webhook := endpointWebhookConfiguration("https://10.40.0.1:9598/mutate/us-east-1/core", []byte("ca")).Webhooks[0]

	if webhook.ClientConfig.URL == nil || *webhook.ClientConfig.URL != "https://10.40.0.1:9598/mutate/us-east-1/core" {
		t.Errorf("clientConfig is %+v", webhook.ClientConfig)
	}
	if webhook.FailurePolicy == nil || *webhook.FailurePolicy != admissionregistrationv1.Fail {
		t.Errorf("failurePolicy is %v, want Fail", webhook.FailurePolicy)
	}
	if webhook.SideEffects == nil || *webhook.SideEffects != admissionregistrationv1.SideEffectClassNoneOnDryRun {
		t.Errorf("sideEffects is %v, want NoneOnDryRun", webhook.SideEffects)
	}
}

func TestTheEndpointWebhookIsNotCalledForKubeSystem(t *testing.T) {
	selector := endpointWebhookConfiguration("https://10.40.0.1:9598/mutate/us-east-1/core", []byte("ca")).Webhooks[0].NamespaceSelector

	excluded := false
	for _, expression := range selector.MatchExpressions {
		if expression.Key == "kubernetes.io/metadata.name" && expression.Operator == metav1.LabelSelectorOpNotIn && contains(expression.Values, "kube-system") {
			excluded = true
		}
	}
	if !excluded {
		t.Fatalf("kube-system is not excluded: %+v", selector)
	}
}

func TestKubeSystemIsSentToTheEndpointWebhookOnlyWhereItsAbsenceIsHarmless(t *testing.T) {
	webhooks := endpointWebhookConfiguration("https://10.40.0.1:9598/mutate/us-east-1/core", []byte("ca")).Webhooks
	if len(webhooks) != 2 {
		t.Fatalf("got %d webhooks, want the general one and the kube-system one", len(webhooks))
	}
	system := webhooks[1]

	selects := false
	for _, expression := range system.NamespaceSelector.MatchExpressions {
		if expression.Key == "kubernetes.io/metadata.name" && expression.Operator == metav1.LabelSelectorOpIn && contains(expression.Values, "kube-system") {
			selects = true
		}
	}
	if !selects {
		t.Errorf("the second webhook does not select kube-system: %+v", system.NamespaceSelector)
	}
	// CoreDNS and the pod identity webhook start while the provisioner is down.
	if system.FailurePolicy == nil || *system.FailurePolicy != admissionregistrationv1.Ignore {
		t.Errorf("failurePolicy is %v, want Ignore", system.FailurePolicy)
	}
	// It decides on the IRSA variables the pod identity webhook adds, so it runs again after it.
	if system.ReinvocationPolicy == nil || *system.ReinvocationPolicy != admissionregistrationv1.IfNeededReinvocationPolicy {
		t.Errorf("reinvocationPolicy is %v, want IfNeeded", system.ReinvocationPolicy)
	}
}

func writeFile(t *testing.T, path, content string) string {
	t.Helper()
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	return path
}

const testPEM = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n"

func TestTheCombinedBundleKeepsTheSystemRootsAndAddsTheEmulatorsCA(t *testing.T) {
	dir := t.TempDir()
	emulatorCA := writeFile(t, filepath.Join(dir, "ca.pem"), strings.ReplaceAll(testPEM, "MIIB", "EMULATORCA00"))
	roots := writeFile(t, filepath.Join(dir, "roots.pem"), strings.ReplaceAll(testPEM, "MIIB", "PUBLICROOT00"))

	bundle, err := loadTrustBundle(emulatorCA, roots)

	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(bundle.Combined), "PUBLICROOT") || !strings.Contains(string(bundle.Combined), "EMULATOR") {
		t.Fatalf("the combined bundle is %q", bundle.Combined)
	}
	if strings.Contains(string(bundle.EmulatorCA), "PUBLICROOT") {
		t.Fatalf("the emulator's CA file holds the system roots")
	}
}

func TestATrustBundleWithoutSystemRootsIsRefused(t *testing.T) {
	dir := t.TempDir()
	emulatorCA := writeFile(t, filepath.Join(dir, "ca.pem"), testPEM)

	if _, err := loadTrustBundle(emulatorCA, filepath.Join(dir, "missing.pem")); err == nil {
		t.Fatal("a missing system root bundle was accepted")
	}
}

func TestTheCAConfigMapIsNamedAfterItsContent(t *testing.T) {
	first := trustBundle{EmulatorCA: []byte("a"), Combined: []byte("roots a")}
	second := trustBundle{EmulatorCA: []byte("b"), Combined: []byte("roots b")}

	if first.configMapName() == second.configMapName() || !strings.HasPrefix(first.configMapName(), "aws-emulator-ca-") {
		t.Fatalf("got %q and %q", first.configMapName(), second.configMapName())
	}
}

func TestEnsuringTheCAConfigMapTwiceKeepsOneImmutableCopy(t *testing.T) {
	clientset := fake.NewSimpleClientset()
	bundle := trustBundle{EmulatorCA: []byte("ca"), Combined: []byte("roots ca")}

	for range 2 {
		if err := ensureTrustBundle(context.Background(), clientset, bundle, "loopzero-system"); err != nil {
			t.Fatal(err)
		}
	}

	created, err := clientset.CoreV1().ConfigMaps("loopzero-system").Get(context.Background(), bundle.configMapName(), metav1.GetOptions{})
	if err != nil {
		t.Fatal(err)
	}
	if created.Immutable == nil || !*created.Immutable || created.Data["ca.pem"] != "ca" || created.Data["ca-bundle.pem"] != "roots ca" {
		t.Fatalf("got %+v", created)
	}
}

func TestOnlyAClusterThisProvisionerBuiltIsTornDownByIt(t *testing.T) {
	p := &provisioner{stateDir: t.TempDir()}
	if err := os.MkdirAll(p.clusterDir("us-east-1", "e2e"), 0o700); err != nil {
		t.Fatal(err)
	}

	if !p.provisioned("us-east-1", "e2e") {
		t.Error("a cluster with a state directory was not taken as this provisioner's")
	}
	if p.provisioned("us-east-1", "loopzero-staging-core") {
		t.Error("a kind cluster another provisioner built was taken as this one's")
	}
}
