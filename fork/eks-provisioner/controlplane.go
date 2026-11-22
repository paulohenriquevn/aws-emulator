// SPDX-License-Identifier: CC0-1.0

package main

import (
	"context"
	"encoding/base64"
	"fmt"
	"net/netip"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/ec2"
	ec2types "github.com/aws/aws-sdk-go-v2/service/ec2/types"
	ekstypes "github.com/aws/aws-sdk-go-v2/service/eks/types"
	corev1 "k8s.io/api/core/v1"
	rbacv1 "k8s.io/api/rbac/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/kubernetes"
	"k8s.io/client-go/tools/clientcmd"
	"sigs.k8s.io/kind/pkg/apis/config/v1alpha4"
	"sigs.k8s.io/kind/pkg/cluster"
)

const (
	// controlPlaneTag names, on the network interface an API server answers at, the
	// cluster it belongs to, as the Amazon EKS-owned interfaces of a cluster are named.
	controlPlaneTag = "aws-emulator:eks-control-plane"
	// apiServerPort is the port the API server answers at on its network interface.
	apiServerPort = 6443
	// podCIDR is the pod network of every cluster: the shared address space Amazon EKS
	// custom networking takes pod addresses from, so it never overlaps a VPC of 10/8.
	podCIDR = "100.64.0.0/16"
	// defaultServiceCIDR is what Amazon EKS picks when the cluster names none.
	defaultServiceCIDR = "10.100.0.0/16"
	// authDir is where the API server reads its token webhook's kubeconfig from.
	authDir = "/etc/kubernetes/eks-auth"
)

// clusterSpec is everything the kind cluster of one EKS cluster is built from.
type clusterSpec struct {
	KindName    string
	Image       string
	Issuer      string
	EndpointIP  netip.Addr
	ServiceCIDR string
	StateDir    string
}

func (spec clusterSpec) caCert() string     { return filepath.Join(spec.StateDir, "pki", "ca.crt") }
func (spec clusterSpec) caKey() string      { return filepath.Join(spec.StateDir, "pki", "ca.key") }
func (spec clusterSpec) authDir() string    { return filepath.Join(spec.StateDir, "eks-auth") }
func (spec clusterSpec) kubeconfig() string { return filepath.Join(spec.StateDir, "kubeconfig") }
func (spec clusterSpec) issuerFile() string { return filepath.Join(spec.StateDir, "issuer") }

// recordIssuer notes which cluster a kind cluster was built for: its OIDC issuer is unique
// to one EKS cluster, so a cluster deleted and created again under the same name has
// another one.
func recordIssuer(spec clusterSpec, issuer string) error {
	if err := os.MkdirAll(spec.StateDir, 0o700); err != nil {
		return err
	}
	return os.WriteFile(spec.issuerFile(), []byte(issuer), 0o644)
}

// builtForAnotherIssuer is whether the kind cluster was built for another EKS cluster of
// the same name (an emulator restart forgets every cluster, and Terraform creates them
// again), so it signs with a CA the new cluster does not report.
func builtForAnotherIssuer(spec clusterSpec, issuer string) bool {
	recorded, err := os.ReadFile(spec.issuerFile())
	return err != nil || strings.TrimSpace(string(recorded)) != issuer
}

// kindConfig is the kind cluster for spec: one control-plane node that signs with the
// cluster's own CA, issues service account tokens as the cluster's OIDC issuer, serves
// its certificate for the address of its network interface, and asks the provisioner's
// webhook who an `aws eks get-token` token belongs to.
func kindConfig(spec clusterSpec) *v1alpha4.Cluster {
	clusterConfiguration := fmt.Sprintf(`kind: ClusterConfiguration
apiServer:
  certSANs:
  - %q
  - "localhost"
  - "127.0.0.1"
  extraArgs:
    service-account-issuer: %q
    authentication-token-webhook-config-file: %q
    authentication-token-webhook-version: v1
  extraVolumes:
  - name: eks-auth
    hostPath: %q
    mountPath: %q
    readOnly: true
    pathType: DirectoryOrCreate
`, spec.EndpointIP, spec.Issuer, authDir+"/webhook.kubeconfig", authDir, authDir)
	return &v1alpha4.Cluster{
		TypeMeta: v1alpha4.TypeMeta{Kind: "Cluster", APIVersion: "kind.x-k8s.io/v1alpha4"},
		Name:     spec.KindName,
		Networking: v1alpha4.Networking{
			PodSubnet:     podCIDR,
			ServiceSubnet: spec.ServiceCIDR,
		},
		Nodes: []v1alpha4.Node{{
			Role:                 v1alpha4.ControlPlaneRole,
			Image:                spec.Image,
			KubeadmConfigPatches: []string{clusterConfiguration},
			ExtraMounts: []v1alpha4.Mount{
				{HostPath: spec.caCert(), ContainerPath: "/etc/kubernetes/pki/ca.crt", Readonly: true},
				{HostPath: spec.caKey(), ContainerPath: "/etc/kubernetes/pki/ca.key", Readonly: true},
				{HostPath: spec.authDir(), ContainerPath: authDir, Readonly: true},
			},
		}},
	}
}

// writeCA stores the cluster's CA where the kind node mounts it from.
func writeCA(spec clusterSpec, plane controlPlane) error {
	certificate, err := base64.StdEncoding.DecodeString(plane.CertificateAuthority.Data)
	if err != nil {
		return fmt.Errorf("the CA data of %s is not Base64: %w", plane.Name, err)
	}
	if plane.CertificateAuthority.SigningKey == "" {
		return fmt.Errorf("the emulator handed out no signing key for the CA of %s", plane.Name)
	}
	if err := os.MkdirAll(filepath.Dir(spec.caCert()), 0o700); err != nil {
		return err
	}
	if err := os.WriteFile(spec.caCert(), certificate, 0o644); err != nil {
		return err
	}
	return os.WriteFile(spec.caKey(), []byte(plane.CertificateAuthority.SigningKey), 0o600)
}

// writeWebhookConfig is the kubeconfig the API server calls the token webhook with.
func writeWebhookConfig(spec clusterSpec, webhookURL string, webhookCA []byte) error {
	if err := os.MkdirAll(spec.authDir(), 0o755); err != nil {
		return err
	}
	content := fmt.Sprintf(`apiVersion: v1
kind: Config
clusters:
- name: eks-auth
  cluster:
    server: %q
    certificate-authority-data: %s
users:
- name: apiserver
contexts:
- name: eks-auth
  context: {cluster: eks-auth, user: apiserver}
current-context: eks-auth
`, webhookURL, base64.StdEncoding.EncodeToString(webhookCA))
	return os.WriteFile(filepath.Join(spec.authDir(), "webhook.kubeconfig"), []byte(content), 0o644)
}

// controlPlaneInterface is the network interface the API server answers at, created in
// the cluster's first subnet with its security groups, as Amazon EKS places its
// control-plane interfaces in the subnets the cluster names. It is found again by tag, so
// a second pass reuses the one the first created.
func (p *provisioner) controlPlaneInterface(ctx context.Context, region, name string, described *ekstypes.Cluster) (netip.Addr, error) {
	client := p.ec2(region)
	tag := region + "/" + name
	existing, err := client.DescribeNetworkInterfaces(ctx, &ec2.DescribeNetworkInterfacesInput{
		Filters: []ec2types.Filter{{Name: aws.String("tag:" + controlPlaneTag), Values: []string{tag}}},
	})
	if err != nil {
		return netip.Addr{}, fmt.Errorf("finding the control-plane interface of %s: %w", name, err)
	}
	for _, eni := range existing.NetworkInterfaces {
		if hasTag(eni.TagSet, controlPlaneTag, tag) {
			return netip.ParseAddr(aws.ToString(eni.PrivateIpAddress))
		}
	}
	if described == nil {
		return netip.Addr{}, fmt.Errorf("cluster %s has no control-plane interface yet", name)
	}
	vpc := described.ResourcesVpcConfig
	if vpc == nil || len(vpc.SubnetIds) == 0 {
		return netip.Addr{}, fmt.Errorf("cluster %s names no subnet", name)
	}
	groups := append([]string{}, vpc.SecurityGroupIds...)
	if id := aws.ToString(vpc.ClusterSecurityGroupId); id != "" {
		groups = append(groups, id)
	}
	created, err := client.CreateNetworkInterface(ctx, &ec2.CreateNetworkInterfaceInput{
		SubnetId:    aws.String(vpc.SubnetIds[0]),
		Description: aws.String("Amazon EKS " + name),
		Groups:      groups,
		TagSpecifications: []ec2types.TagSpecification{{
			ResourceType: ec2types.ResourceTypeNetworkInterface,
			Tags:         []ec2types.Tag{{Key: aws.String(controlPlaneTag), Value: aws.String(tag)}},
		}},
	})
	if err != nil {
		return netip.Addr{}, fmt.Errorf("creating the control-plane interface of %s: %w", name, err)
	}
	return netip.ParseAddr(aws.ToString(created.NetworkInterface.PrivateIpAddress))
}

func (p *provisioner) deleteControlPlaneInterface(ctx context.Context, region, name string) error {
	client := p.ec2(region)
	tag := region + "/" + name
	existing, err := client.DescribeNetworkInterfaces(ctx, &ec2.DescribeNetworkInterfacesInput{
		Filters: []ec2types.Filter{{Name: aws.String("tag:" + controlPlaneTag), Values: []string{tag}}},
	})
	if err != nil {
		return err
	}
	for _, eni := range existing.NetworkInterfaces {
		if !hasTag(eni.TagSet, controlPlaneTag, tag) {
			continue
		}
		if _, err := client.DeleteNetworkInterface(ctx, &ec2.DeleteNetworkInterfaceInput{NetworkInterfaceId: eni.NetworkInterfaceId}); err != nil {
			return fmt.Errorf("deleting %s: %w", aws.ToString(eni.NetworkInterfaceId), err)
		}
	}
	return nil
}

// provisionControlPlane brings up the API server of a cluster and registers it. Every step
// is safe to repeat, so a pass that fails halfway is finished by the next one.
func (p *provisioner) provisionControlPlane(ctx context.Context, region string, described *ekstypes.Cluster) error {
	name := aws.ToString(described.Name)
	if !kindNamePattern.MatchString(name) || len(kindName(region, name)) > 63 {
		return fmt.Errorf("cluster name %q cannot name a kind cluster: use lowercase letters, digits, '-' and '.', at most %d characters", name, 63-len(kindName(region, "")))
	}
	plane, err := p.emulator.controlPlane(ctx, region, name)
	if err != nil {
		return err
	}
	image, err := nodeImage(plane.Version)
	if err != nil {
		return err
	}
	vpcID := aws.ToString(described.ResourcesVpcConfig.VpcId)
	cidr, subnets, err := p.vpcLayout(ctx, region, vpcID)
	if err != nil {
		return err
	}
	network, err := vpcNetwork(ctx, vpcID, cidr, subnets)
	if err != nil {
		return err
	}
	endpointIP, err := p.controlPlaneInterface(ctx, region, name, described)
	if err != nil {
		return err
	}
	serviceCIDR := defaultServiceCIDR
	if config := described.KubernetesNetworkConfig; config != nil && aws.ToString(config.ServiceIpv4Cidr) != "" {
		serviceCIDR = aws.ToString(config.ServiceIpv4Cidr)
	}
	spec := clusterSpec{
		KindName:    kindName(region, name),
		Image:       image,
		Issuer:      plane.Issuer,
		EndpointIP:  endpointIP,
		ServiceCIDR: serviceCIDR,
		StateDir:    p.clusterDir(region, name),
	}
	if p.kindExists(spec.KindName) && builtForAnotherIssuer(spec, plane.Issuer) {
		p.log.Info("the control plane was built for an earlier cluster of the same name; rebuilding it", "cluster", name, "region", region)
		if err := p.kind.Delete(spec.KindName, spec.kubeconfig()); err != nil {
			return fmt.Errorf("kind delete cluster %s: %w", spec.KindName, err)
		}
	}
	if err := writeCA(spec, plane); err != nil {
		return err
	}
	if err := writeWebhookConfig(spec, p.webhookURL(gatewayOf(cidr), region, name), p.webhook.caPEM); err != nil {
		return err
	}
	if !p.kindExists(spec.KindName) {
		p.log.Info("creating the control plane", "cluster", name, "region", region, "image", image, "endpoint", endpointIP, "network", network)
		// kind reads the network to create nodes on from its environment; the passes of
		// the reconcile loop are sequential, so no other create reads it meanwhile.
		os.Setenv("KIND_EXPERIMENTAL_DOCKER_NETWORK", network)
		err := p.kind.Create(spec.KindName,
			cluster.CreateWithV1Alpha4Config(kindConfig(spec)),
			cluster.CreateWithKubeconfigPath(spec.kubeconfig()),
			cluster.CreateWithWaitForReady(3*time.Minute),
			cluster.CreateWithDisplayUsage(false),
			cluster.CreateWithDisplaySalutation(false),
		)
		if err != nil {
			return fmt.Errorf("kind create cluster %s: %w", spec.KindName, err)
		}
		if err := recordIssuer(spec, plane.Issuer); err != nil {
			return err
		}
	}
	controlPlaneNode := spec.KindName + "-control-plane"
	if err := answerAt(ctx, controlPlaneNode, endpointIP); err != nil {
		return err
	}
	clientset, err := p.clientset(region, name)
	if err != nil {
		return err
	}
	if err := keepWorkloadsOffTheControlPlane(ctx, clientset, controlPlaneNode); err != nil {
		return err
	}
	if err := bindAccessPolicies(ctx, clientset); err != nil {
		return err
	}
	jwks, err := clientset.CoreV1().RESTClient().Get().AbsPath("/openid/v1/jwks").DoRaw(ctx)
	if err != nil {
		return fmt.Errorf("reading the service account key set of %s: %w", name, err)
	}
	if err := p.emulator.registerKeySet(ctx, region, name, jwks); err != nil {
		return err
	}
	endpoint := fmt.Sprintf("https://%s:%d", endpointIP, apiServerPort)
	if err := p.emulator.registerEndpoint(ctx, region, name, endpoint); err != nil {
		return err
	}
	p.log.Info("control plane registered", "cluster", name, "region", region, "endpoint", endpoint)
	return nil
}

// answerAt adds the interface's address to the control-plane node, which the host reaches
// over the VPC's bridge.
func answerAt(ctx context.Context, node string, address netip.Addr) error {
	addresses, err := docker(ctx, "exec", node, "ip", "-4", "-o", "addr", "show", "dev", "eth0")
	if err != nil {
		return err
	}
	if containsField(addresses, address.String()+"/32") {
		return nil
	}
	_, err = docker(ctx, "exec", node, "ip", "addr", "add", address.String()+"/32", "dev", "eth0")
	return err
}

// keepWorkloadsOffTheControlPlane taints the control-plane node: on Amazon EKS no pod of
// the customer runs on the control plane, and kind untaints a cluster with no workers.
func keepWorkloadsOffTheControlPlane(ctx context.Context, clientset kubernetes.Interface, nodeName string) error {
	node, err := clientset.CoreV1().Nodes().Get(ctx, nodeName, metav1.GetOptions{})
	if err != nil {
		return fmt.Errorf("reading node %s: %w", nodeName, err)
	}
	taint := corev1.Taint{Key: "node-role.kubernetes.io/control-plane", Effect: corev1.TaintEffectNoSchedule}
	for _, existing := range node.Spec.Taints {
		if existing.Key == taint.Key && existing.Effect == taint.Effect {
			return nil
		}
	}
	node.Spec.Taints = append(node.Spec.Taints, taint)
	_, err = clientset.CoreV1().Nodes().Update(ctx, node, metav1.UpdateOptions{})
	return err
}

// bindAccessPolicies binds the group of each cluster-scoped access policy to the
// ClusterRole the policy grants.
func bindAccessPolicies(ctx context.Context, clientset kubernetes.Interface) error {
	for policy, role := range policyClusterRoles {
		group := "eks-access-policy:" + policy
		binding := &rbacv1.ClusterRoleBinding{
			ObjectMeta: metav1.ObjectMeta{Name: "eks-access-policy-" + role, Labels: map[string]string{"aws-emulator/access-policy": policy}},
			RoleRef:    rbacv1.RoleRef{APIGroup: "rbac.authorization.k8s.io", Kind: "ClusterRole", Name: role},
			Subjects:   []rbacv1.Subject{{APIGroup: "rbac.authorization.k8s.io", Kind: "Group", Name: group}},
		}
		if _, err := clientset.RbacV1().ClusterRoleBindings().Create(ctx, binding, metav1.CreateOptions{}); err != nil && !apierrors.IsAlreadyExists(err) {
			return fmt.Errorf("binding %s to %s: %w", group, role, err)
		}
	}
	return nil
}

// bindNamespacedPolicy binds a namespace-scoped policy's group to its ClusterRole in one
// namespace. A namespace that does not exist yet is skipped; the next token review of
// the principal binds it.
func bindNamespacedPolicy(ctx context.Context, clientset kubernetes.Interface, policy, namespace string) error {
	role := policyClusterRoles[policy]
	group := "eks-access-policy:" + policy + ":" + namespace
	binding := &rbacv1.RoleBinding{
		ObjectMeta: metav1.ObjectMeta{Name: "eks-access-policy-" + role, Namespace: namespace, Labels: map[string]string{"aws-emulator/access-policy": policy}},
		RoleRef:    rbacv1.RoleRef{APIGroup: "rbac.authorization.k8s.io", Kind: "ClusterRole", Name: role},
		Subjects:   []rbacv1.Subject{{APIGroup: "rbac.authorization.k8s.io", Kind: "Group", Name: group}},
	}
	_, err := clientset.RbacV1().RoleBindings(namespace).Create(ctx, binding, metav1.CreateOptions{})
	if err == nil || apierrors.IsAlreadyExists(err) || apierrors.IsNotFound(err) {
		return nil
	}
	return fmt.Errorf("binding %s to %s in %s: %w", group, role, namespace, err)
}

// clientset is an administrator's client of the cluster's API server, from the kubeconfig
// kind wrote.
func (p *provisioner) clientset(region, name string) (kubernetes.Interface, error) {
	config, err := clientcmd.BuildConfigFromFlags("", filepath.Join(p.clusterDir(region, name), "kubeconfig"))
	if err != nil {
		return nil, fmt.Errorf("the kubeconfig of %s: %w", name, err)
	}
	config.Timeout = 30 * time.Second
	return kubernetes.NewForConfig(config)
}

// vpcLayout is the CIDR of a VPC and of its subnets.
func (p *provisioner) vpcLayout(ctx context.Context, region, vpcID string) (netip.Prefix, []netip.Prefix, error) {
	client := p.ec2(region)
	vpcs, err := client.DescribeVpcs(ctx, &ec2.DescribeVpcsInput{VpcIds: []string{vpcID}})
	if err != nil || len(vpcs.Vpcs) == 0 {
		return netip.Prefix{}, nil, fmt.Errorf("describing VPC %s: %v", vpcID, err)
	}
	cidr, err := netip.ParsePrefix(aws.ToString(vpcs.Vpcs[0].CidrBlock))
	if err != nil {
		return netip.Prefix{}, nil, fmt.Errorf("VPC %s CIDR: %w", vpcID, err)
	}
	found, err := client.DescribeSubnets(ctx, &ec2.DescribeSubnetsInput{Filters: []ec2types.Filter{{Name: aws.String("vpc-id"), Values: []string{vpcID}}}})
	if err != nil {
		return netip.Prefix{}, nil, fmt.Errorf("describing the subnets of %s: %w", vpcID, err)
	}
	var subnets []netip.Prefix
	for _, subnet := range found.Subnets {
		prefix, err := netip.ParsePrefix(aws.ToString(subnet.CidrBlock))
		if err != nil {
			return netip.Prefix{}, nil, fmt.Errorf("subnet %s CIDR: %w", aws.ToString(subnet.SubnetId), err)
		}
		subnets = append(subnets, prefix)
	}
	return cidr, subnets, nil
}

func hasTag(tags []ec2types.Tag, key, value string) bool {
	for _, tag := range tags {
		if aws.ToString(tag.Key) == key && aws.ToString(tag.Value) == value {
			return true
		}
	}
	return false
}

func containsField(text, field string) bool {
	for _, candidate := range strings.Fields(text) {
		if candidate == field {
			return true
		}
	}
	return false
}
