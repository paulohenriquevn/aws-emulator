// SPDX-License-Identifier: CC0-1.0

package main

import (
	"net/netip"
	"strings"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	ekstypes "github.com/aws/aws-sdk-go-v2/service/eks/types"
)

func testSpec() clusterSpec {
	return clusterSpec{
		KindName:    "eks-us-east-1-core",
		Image:       nodeImages["1.34"],
		Issuer:      "https://oidc.eks.us-east-1.amazonaws.com/id/ABC",
		EndpointIP:  netip.MustParseAddr("10.0.1.20"),
		ServiceCIDR: "172.20.0.0/16",
		StateDir:    "/var/lib/aws-emulator-eks/clusters/us-east-1/core",
	}
}

func TestTheControlPlaneSignsWithTheClustersCA(t *testing.T) {
	mounts := kindConfig(testSpec()).Nodes[0].ExtraMounts

	want := map[string]string{
		"/etc/kubernetes/pki/ca.crt": "/var/lib/aws-emulator-eks/clusters/us-east-1/core/pki/ca.crt",
		"/etc/kubernetes/pki/ca.key": "/var/lib/aws-emulator-eks/clusters/us-east-1/core/pki/ca.key",
	}
	for _, mount := range mounts {
		if host, ok := want[mount.ContainerPath]; ok && host == mount.HostPath {
			delete(want, mount.ContainerPath)
		}
	}
	if len(want) != 0 {
		t.Fatalf("missing mounts %v in %+v", want, mounts)
	}
}

func TestTheAPIServerIssuesTokensAsTheClustersIssuerAndServesItsInterfaceAddress(t *testing.T) {
	patch := kindConfig(testSpec()).Nodes[0].KubeadmConfigPatches[0]

	for _, fragment := range []string{
		`service-account-issuer: "https://oidc.eks.us-east-1.amazonaws.com/id/ABC"`,
		`- "10.0.1.20"`,
		`authentication-token-webhook-config-file: "/etc/kubernetes/eks-auth/webhook.kubeconfig"`,
	} {
		if !strings.Contains(patch, fragment) {
			t.Errorf("the ClusterConfiguration patch lacks %s:\n%s", fragment, patch)
		}
	}
}

func TestTheClusterUsesTheServiceCIDRItWasCreatedWith(t *testing.T) {
	if got := kindConfig(testSpec()).Networking.ServiceSubnet; got != "172.20.0.0/16" {
		t.Fatalf("got %q", got)
	}
}

func testRegistration() nodeRegistration {
	return nodeRegistration{
		Instance: nodeInstance{ID: "i-0abc", Address: "10.0.1.7", Hostname: "ip-10-0-1-7.ec2.internal", Zone: "us-east-1b", InstanceType: "t3.large", Nodegroup: "workers"},
		Region:   "us-east-1",
		Labels:   map[string]string{"role": "general"},
		Taints:   []string{"dedicated=build:NoSchedule"},
	}
}

func TestANodeRegistersAsItsInstance(t *testing.T) {
	config := joinConfiguration(testRegistration(), "10.0.1.20:6443", "abcdef.0123456789abcdef", "sha256:00")

	for _, fragment := range []string{
		`name: "ip-10-0-1-7.ec2.internal"`,
		`{name: provider-id, value: "aws:///us-east-1b/i-0abc"}`,
		`{name: node-ip, value: "10.0.1.7"}`,
		`apiServerEndpoint: "10.0.1.20:6443"`,
	} {
		if !strings.Contains(config, fragment) {
			t.Errorf("the JoinConfiguration lacks %s:\n%s", fragment, config)
		}
	}
}

func TestANodeCarriesItsNodegroupsLabelsAndTaints(t *testing.T) {
	config := joinConfiguration(testRegistration(), "10.0.1.20:6443", "abcdef.0123456789abcdef", "sha256:00")

	for _, fragment := range []string{"eks.amazonaws.com/nodegroup=workers", "role=general", "topology.kubernetes.io/zone=us-east-1b", `register-with-taints, value: "dedicated=build:NoSchedule"`} {
		if !strings.Contains(config, fragment) {
			t.Errorf("the JoinConfiguration lacks %s:\n%s", fragment, config)
		}
	}
}

func TestNodegroupTaintsAreWrittenTheWayKubeletReadsThem(t *testing.T) {
	got := nodegroupTaints([]ekstypes.Taint{
		{Key: aws.String("dedicated"), Value: aws.String("build"), Effect: ekstypes.TaintEffectNoSchedule},
		{Key: aws.String("spot"), Effect: ekstypes.TaintEffectPreferNoSchedule},
	})

	if strings.Join(got, ",") != "dedicated=build:NoSchedule,spot:PreferNoSchedule" {
		t.Fatalf("got %v", got)
	}
}

func TestAClusterIsRegisteredOnlyAtAnAPIServerThisProvisionerRuns(t *testing.T) {
	p := &provisioner{}
	cases := map[string]bool{
		"https://10.0.1.20:6443":                         true,
		"https://ABCDEF.gr7.us-east-1.eks.amazonaws.com": false,
		"": false,
	}
	for endpoint, want := range cases {
		if got := p.registered(&ekstypes.Cluster{Endpoint: aws.String(endpoint)}); got != want {
			t.Errorf("%q: got %v, want %v", endpoint, got, want)
		}
	}
}

func TestAClusterNameKindCannotCarryIsRecognised(t *testing.T) {
	for name, valid := range map[string]bool{"core": true, "cell-0": true, "Core": false, "my_cluster": false} {
		if got := kindNamePattern.MatchString(name); got != valid {
			t.Errorf("%q: got %v, want %v", name, got, valid)
		}
	}
}

func TestAControlPlaneBuiltForAnotherIssuerIsStale(t *testing.T) {
	dir := t.TempDir()
	spec := clusterSpec{StateDir: dir}
	if err := recordIssuer(spec, "https://oidc.eks.us-east-1.amazonaws.com/id/OLD"); err != nil {
		t.Fatal(err)
	}

	if !builtForAnotherIssuer(spec, "https://oidc.eks.us-east-1.amazonaws.com/id/NEW") {
		t.Fatal("a control plane built for the old issuer was kept for the new one")
	}
	if builtForAnotherIssuer(spec, "https://oidc.eks.us-east-1.amazonaws.com/id/OLD") {
		t.Fatal("a control plane built for this issuer was taken as stale")
	}
}

func TestAControlPlaneWithNoRecordedIssuerIsStale(t *testing.T) {
	if !builtForAnotherIssuer(clusterSpec{StateDir: t.TempDir()}, "https://oidc.eks.us-east-1.amazonaws.com/id/NEW") {
		t.Fatal("a control plane of unknown origin was kept")
	}
}
