// SPDX-License-Identifier: CC0-1.0

package main

import (
	"bytes"
	"context"
	"fmt"
	"os/exec"
	"regexp"
	"sort"
	"strings"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/ec2"
	ec2types "github.com/aws/aws-sdk-go-v2/service/ec2/types"
	"github.com/aws/aws-sdk-go-v2/service/eks"
	ekstypes "github.com/aws/aws-sdk-go-v2/service/eks/types"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// instanceLabel names, on a node container, the EC2 instance the container is.
const instanceLabel = "aws-emulator.instance-id"

// nodeInstance is an EC2 instance of one of a cluster's managed node groups.
type nodeInstance struct {
	ID           string
	Address      string
	Hostname     string
	Zone         string
	InstanceType string
	Nodegroup    string
}

// nodeRegistration is what the kubelet of an instance registers with: the node name, the
// providerID, the address, and the labels and taints of its node group.
type nodeRegistration struct {
	Instance nodeInstance
	Region   string
	Labels   map[string]string
	Taints   []string
}

// joinConfiguration is the kubeadm JoinConfiguration of an instance.
func joinConfiguration(registration nodeRegistration, apiServer, token, caHash string) string {
	instance := registration.Instance
	labels := map[string]string{
		"eks.amazonaws.com/nodegroup":      instance.Nodegroup,
		"eks.amazonaws.com/capacityType":   "ON_DEMAND",
		"node.kubernetes.io/instance-type": instance.InstanceType,
		"topology.kubernetes.io/region":    registration.Region,
		"topology.kubernetes.io/zone":      instance.Zone,
	}
	for key, value := range registration.Labels {
		labels[key] = value
	}
	keys := make([]string, 0, len(labels))
	for key := range labels {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	pairs := make([]string, 0, len(keys))
	for _, key := range keys {
		pairs = append(pairs, key+"="+labels[key])
	}
	args := fmt.Sprintf(`  - {name: provider-id, value: "aws:///%s/%s"}
  - {name: node-ip, value: %q}
  - {name: node-labels, value: %q}
  - {name: image-credential-provider-config, value: %q}
  - {name: image-credential-provider-bin-dir, value: %q}
`, instance.Zone, instance.ID, instance.Address, strings.Join(pairs, ","), credentialProviderConfigPath, credentialProviderDir)
	if len(registration.Taints) > 0 {
		args += fmt.Sprintf("  - {name: register-with-taints, value: %q}\n", strings.Join(registration.Taints, ","))
	}
	return fmt.Sprintf(`apiVersion: kubeadm.k8s.io/v1beta4
kind: JoinConfiguration
discovery:
  bootstrapToken:
    apiServerEndpoint: %q
    token: %q
    caCertHashes: [%q]
nodeRegistration:
  name: %q
  criSocket: unix:///run/containerd/containerd.sock
  kubeletExtraArgs:
%s`, apiServer, token, caHash, instance.Hostname, args)
}

// nodegroupTaints is a node group's taints in the form kubelet registers them with.
func nodegroupTaints(taints []ekstypes.Taint) []string {
	effects := map[ekstypes.TaintEffect]string{
		ekstypes.TaintEffectNoSchedule:       "NoSchedule",
		ekstypes.TaintEffectNoExecute:        "NoExecute",
		ekstypes.TaintEffectPreferNoSchedule: "PreferNoSchedule",
	}
	var out []string
	for _, taint := range taints {
		entry := aws.ToString(taint.Key)
		if value := aws.ToString(taint.Value); value != "" {
			entry += "=" + value
		}
		out = append(out, entry+":"+effects[taint.Effect])
	}
	return out
}

var joinCommand = regexp.MustCompile(`--token\s+(\S+)\s+--discovery-token-ca-cert-hash\s+(\S+)`)

// reconcileNodes makes the node containers of a cluster match the running instances of
// its node groups: an instance with no container is created and joined, and a container
// whose instance is gone is removed with its Node.
func (p *provisioner) reconcileNodes(ctx context.Context, region, name string) error {
	instances, err := p.nodegroupInstances(ctx, region, name)
	if err != nil {
		return err
	}
	kind := kindName(region, name)
	listed, err := docker(ctx, "ps", "-a", "--filter", "label=io.x-k8s.kind.cluster="+kind, "--filter", "label="+instanceLabel,
		"--format", `{{.Label "`+instanceLabel+`"}} {{.Names}}`)
	if err != nil {
		return err
	}
	containers := map[string]string{}
	for _, line := range strings.Split(listed, "\n") {
		if id, container, ok := strings.Cut(strings.TrimSpace(line), " "); ok {
			containers[id] = container
		}
	}
	wanted := map[string]bool{}
	var failures []string
	for _, instance := range instances {
		wanted[instance.ID] = true
		if container, ok := containers[instance.ID]; ok {
			if err := p.ensureImagePulls(ctx, region, name, container); err != nil {
				failures = append(failures, err.Error())
			}
			continue
		}
		if err := p.joinInstance(ctx, region, name, instance); err != nil {
			failures = append(failures, err.Error())
		}
	}
	for id, container := range containers {
		if wanted[id] {
			continue
		}
		if err := p.removeNode(ctx, region, name, container); err != nil {
			failures = append(failures, err.Error())
		}
	}
	if len(failures) > 0 {
		return fmt.Errorf("nodes of %s: %s", name, strings.Join(failures, "; "))
	}
	return nil
}

// nodegroupInstances are the running EC2 instances the cluster's managed node groups
// launched, which Amazon EKS tags with the cluster and the node group.
func (p *provisioner) nodegroupInstances(ctx context.Context, region, name string) ([]nodeInstance, error) {
	paginator := ec2.NewDescribeInstancesPaginator(p.ec2(region), &ec2.DescribeInstancesInput{Filters: []ec2types.Filter{
		{Name: aws.String("tag:eks:cluster-name"), Values: []string{name}},
		{Name: aws.String("instance-state-name"), Values: []string{"running"}},
	}})
	var instances []nodeInstance
	for paginator.HasMorePages() {
		page, err := paginator.NextPage(ctx)
		if err != nil {
			return nil, fmt.Errorf("the instances of %s: %w", name, err)
		}
		for _, reservation := range page.Reservations {
			for _, instance := range reservation.Instances {
				nodegroup := tagValue(instance.Tags, "eks:nodegroup-name")
				if nodegroup == "" || tagValue(instance.Tags, "eks:cluster-name") != name {
					continue
				}
				hostname := aws.ToString(instance.PrivateDnsName)
				if hostname == "" {
					hostname = aws.ToString(instance.InstanceId)
				}
				instances = append(instances, nodeInstance{
					ID:           aws.ToString(instance.InstanceId),
					Address:      aws.ToString(instance.PrivateIpAddress),
					Hostname:     hostname,
					Zone:         aws.ToString(instance.Placement.AvailabilityZone),
					InstanceType: string(instance.InstanceType),
					Nodegroup:    nodegroup,
				})
			}
		}
	}
	return instances, nil
}

// joinInstance runs a kind node container as the instance, at the instance's address on
// the VPC's network, and joins it to the cluster with a bootstrap token.
func (p *provisioner) joinInstance(ctx context.Context, region, name string, instance nodeInstance) error {
	kind := kindName(region, name)
	controlPlane := kind + "-control-plane"
	image, err := docker(ctx, "inspect", "-f", "{{.Config.Image}}", controlPlane)
	if err != nil {
		return err
	}
	network, err := docker(ctx, "inspect", "-f", "{{range $name, $_ := .NetworkSettings.Networks}}{{$name}} {{end}}", controlPlane)
	if err != nil {
		return err
	}
	nodegroup, err := p.eks(region).DescribeNodegroup(ctx, &eks.DescribeNodegroupInput{ClusterName: aws.String(name), NodegroupName: aws.String(instance.Nodegroup)})
	if err != nil {
		return fmt.Errorf("describing node group %s: %w", instance.Nodegroup, err)
	}
	container := kind + "-" + instance.ID
	p.log.Info("joining an instance", "cluster", name, "instance", instance.ID, "address", instance.Address, "nodegroup", instance.Nodegroup)
	_, err = docker(ctx, "run", "-d", "--name", container, "--hostname", instance.Hostname,
		"--privileged", "--security-opt", "seccomp=unconfined", "--security-opt", "apparmor=unconfined",
		"--tmpfs", "/tmp", "--tmpfs", "/run", "--volume", "/var", "--volume", "/lib/modules:/lib/modules:ro",
		"--cgroupns=private", "--restart=on-failure:1", "--init=false",
		"--network", strings.Fields(network)[0], "--ip", instance.Address,
		"--label", "io.x-k8s.kind.cluster="+kind, "--label", "io.x-k8s.kind.role=worker", "--label", instanceLabel+"="+instance.ID,
		"--tty", image)
	if err != nil {
		return err
	}
	if err := p.join(ctx, region, name, container, controlPlane, nodeRegistration{
		Instance: instance,
		Region:   region,
		Labels:   nodegroup.Nodegroup.Labels,
		Taints:   nodegroupTaints(nodegroup.Nodegroup.Taints),
	}); err != nil {
		// Removed so the next pass starts the instance over instead of finding a container
		// that never joined.
		if _, removeErr := docker(context.WithoutCancel(ctx), "rm", "-f", container); removeErr != nil {
			return fmt.Errorf("%w; removing %s: %v", err, container, removeErr)
		}
		return err
	}
	return nil
}

func (p *provisioner) join(ctx context.Context, region, name, container, controlPlane string, registration nodeRegistration) error {
	if err := waitFor(ctx, 90*time.Second, container+" starting containerd", func() error {
		state, err := docker(ctx, "exec", container, "systemctl", "is-active", "containerd")
		if err != nil || state != "active" {
			return fmt.Errorf("containerd is %q", state)
		}
		return nil
	}); err != nil {
		return err
	}
	printed, err := docker(ctx, "exec", controlPlane, "kubeadm", "token", "create", "--ttl", "15m", "--print-join-command")
	if err != nil {
		return err
	}
	match := joinCommand.FindStringSubmatch(printed)
	if match == nil {
		return fmt.Errorf("kubeadm printed no token and CA hash: %q", printed)
	}
	endpoint, err := p.controlPlaneInterface(ctx, region, name, nil)
	if err != nil {
		return err
	}
	// The kubelet starts with the image credential provider, so its files are there first.
	pulls, err := p.nodePulls(ctx, region, name, container)
	if err != nil {
		return err
	}
	if err := configureImagePulls(ctx, container, pulls); err != nil {
		return err
	}
	config := joinConfiguration(registration, fmt.Sprintf("%s:%d", endpoint, apiServerPort), match[1], match[2])
	if err := dockerWithInput(ctx, config, "exec", "-i", container, "sh", "-c", "cat > /kind/join.yaml"); err != nil {
		return err
	}
	if _, err := docker(ctx, "exec", container, "kubeadm", "join", "--config", "/kind/join.yaml", "--skip-phases=preflight"); err != nil {
		return fmt.Errorf("kubeadm join of %s: %w", registration.Instance.ID, err)
	}
	p.pullsConfigured[container] = true
	return nil
}

func (p *provisioner) removeNode(ctx context.Context, region, name, container string) error {
	hostname, err := docker(ctx, "inspect", "-f", "{{.Config.Hostname}}", container)
	if err != nil {
		return err
	}
	clientset, err := p.clientset(region, name)
	if err != nil {
		return err
	}
	if err := clientset.CoreV1().Nodes().Delete(ctx, hostname, metav1.DeleteOptions{}); err != nil && !apierrors.IsNotFound(err) {
		return fmt.Errorf("deleting node %s: %w", hostname, err)
	}
	p.log.Info("removed a node whose instance is gone", "cluster", name, "node", hostname)
	delete(p.pullsConfigured, container)
	_, err = docker(ctx, "rm", "-f", container)
	return err
}

func dockerWithInput(ctx context.Context, input string, args ...string) error {
	var stderr bytes.Buffer
	command := exec.CommandContext(ctx, "docker", args...)
	command.Stdin = strings.NewReader(input)
	command.Stderr = &stderr
	if err := command.Run(); err != nil {
		return fmt.Errorf("docker %s: %w: %s", strings.Join(args, " "), err, strings.TrimSpace(stderr.String()))
	}
	return nil
}

func tagValue(tags []ec2types.Tag, key string) string {
	for _, tag := range tags {
		if aws.ToString(tag.Key) == key {
			return aws.ToString(tag.Value)
		}
	}
	return ""
}
