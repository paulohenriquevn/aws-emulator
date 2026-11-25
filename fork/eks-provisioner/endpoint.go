// SPDX-License-Identifier: CC0-1.0

package main

import (
	"context"
	"fmt"
	"net"
	"net/url"
	"time"
)

const (
	// emulatorHostname is the name pods reach the emulator by. Each pod resolves it, and
	// its account-prefixed form, to the gateway through its /etc/hosts (hostAliases), and
	// the emulator's certificate names both. A name rather than the gateway's address,
	// because an SDK prefixes the endpoint's host with the account id for some services (the
	// CloudFront KeyValueStore data plane, S3 Control), which an address cannot carry.
	emulatorHostname = "aws.emulator.internal"
	// emulatorAccountID is the one account the emulator holds, which those SDKs prefix.
	emulatorAccountID = "000000000000"
)

// emulatorHostnames are the names a pod's /etc/hosts maps to the gateway.
func emulatorHostnames() []string {
	return []string{emulatorHostname, emulatorAccountID + "." + emulatorHostname}
}

// podGateway is the address and port pods reach the emulator at: those of the provisioner's
// own AWS_ENDPOINT_URL, which names the gateway of the emulator's network. A name is
// resolved once, here, since a pod's /etc/hosts holds an address.
func podGateway(endpoint string) (string, string, error) {
	parsed, err := url.Parse(endpoint)
	if err != nil || parsed.Hostname() == "" {
		return "", "", fmt.Errorf("AWS_ENDPOINT_URL %q names no host", endpoint)
	}
	port := parsed.Port()
	if port == "" {
		port = "443"
	}
	host := parsed.Hostname()
	if ip := net.ParseIP(host); ip != nil {
		return ip.String(), port, nil
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	addresses, err := net.DefaultResolver.LookupIPAddr(ctx, host)
	if err != nil || len(addresses) == 0 {
		return "", "", fmt.Errorf("AWS_ENDPOINT_URL's host %s does not resolve to the address pods reach the emulator at: %v", host, err)
	}
	for _, address := range addresses {
		if address.IP.To4() != nil {
			return address.IP.String(), port, nil
		}
	}
	return addresses[0].IP.String(), port, nil
}
