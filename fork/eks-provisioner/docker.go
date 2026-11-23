// SPDX-License-Identifier: CC0-1.0

package main

import (
	"bytes"
	"context"
	"fmt"
	"net/netip"
	"os/exec"
	"strings"
	"time"
)

// docker runs the Docker CLI, as kind itself does.
func docker(ctx context.Context, args ...string) (string, error) {
	var stdout, stderr bytes.Buffer
	command := exec.CommandContext(ctx, "docker", args...)
	command.Stdout, command.Stderr = &stdout, &stderr
	if err := command.Run(); err != nil {
		return "", fmt.Errorf("docker %s: %w: %s", strings.Join(args, " "), err, strings.TrimSpace(stderr.String()))
	}
	return strings.TrimSpace(stdout.String()), nil
}

// vpcNetwork makes sure a Docker bridge network stands for the VPC and returns its name.
//
// A network that already has exactly the VPC's CIDR is that VPC's network whatever its
// name, so a network created before the provisioner ran (fork/kubernetes/run.sh creates
// one for the emulator) is adopted rather than fought over.
func vpcNetwork(ctx context.Context, vpcID string, cidr netip.Prefix, subnets []netip.Prefix) (string, error) {
	names, err := docker(ctx, "network", "ls", "--format", "{{.Name}}")
	if err != nil {
		return "", err
	}
	for _, name := range strings.Fields(names) {
		subnet, err := docker(ctx, "network", "inspect", name, "--format", "{{range .IPAM.Config}}{{.Subnet}} {{end}}")
		if err != nil {
			continue
		}
		for _, existing := range strings.Fields(subnet) {
			if existing == cidr.Masked().String() {
				return name, nil
			}
		}
	}
	ipRange, err := dockerAddressRange(cidr, subnets)
	if err != nil {
		return "", fmt.Errorf("VPC %s: %w", vpcID, err)
	}
	name := vpcNetworkName(vpcID, cidr)
	_, err = docker(ctx, "network", "create", name, "--driver", "bridge",
		"--subnet", cidr.Masked().String(), "--ip-range", ipRange.String(), "--gateway", gatewayOf(cidr).String(),
		"--label", "aws-emulator.vpc-id="+vpcID,
		"-o", "com.docker.network.bridge.enable_ip_masquerade=true")
	if err != nil {
		return "", err
	}
	return name, nil
}

// vpcNetworkName is the Docker network created for a VPC. The CIDR is in the name because
// two emulators on one host hand out the same VPC ids, and Docker keeps the CIDR unique.
func vpcNetworkName(vpcID string, cidr netip.Prefix) string {
	return "aws-emulator-" + vpcID + "-" + strings.ReplaceAll(cidr.Masked().String(), "/", "-")
}

// waitFor runs probe until it succeeds or timeout passes.
func waitFor(ctx context.Context, timeout time.Duration, what string, probe func() error) error {
	deadline := time.Now().Add(timeout)
	for {
		err := probe()
		if err == nil {
			return nil
		}
		if time.Now().After(deadline) {
			return fmt.Errorf("%s did not happen within %s: %w", what, timeout, err)
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(time.Second):
		}
	}
}
