// SPDX-License-Identifier: CC0-1.0

package main

import (
	"strings"
	"testing"
)

func TestTheGatewayIsTheAddressTheEndpointNames(t *testing.T) {
	gateway, port, err := podGateway("https://172.29.0.1:9597")

	if err != nil || gateway != "172.29.0.1" || port != "9597" {
		t.Fatalf("got %q %q %v", gateway, port, err)
	}
}

func TestAnEndpointWithNoPortIsReachedOn443(t *testing.T) {
	_, port, err := podGateway("https://172.29.0.1")

	if err != nil || port != "443" {
		t.Fatalf("got %q %v", port, err)
	}
}

func TestAnEndpointNamedByHostnameIsResolvedToItsAddress(t *testing.T) {
	gateway, _, err := podGateway("https://localhost:9597")

	if err != nil || (gateway != "127.0.0.1" && gateway != "::1") {
		t.Fatalf("got %q %v", gateway, err)
	}
}

func TestAnEndpointWhoseNameDoesNotResolveIsAnError(t *testing.T) {
	_, _, err := podGateway("https://emulator.invalid:9597")

	if err == nil || !strings.Contains(err.Error(), "emulator.invalid") {
		t.Fatalf("got %v", err)
	}
}
