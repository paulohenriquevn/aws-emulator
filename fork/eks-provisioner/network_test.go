// SPDX-License-Identifier: CC0-1.0

package main

import (
	"net/netip"
	"testing"
)

func prefixes(texts ...string) []netip.Prefix {
	var out []netip.Prefix
	for _, text := range texts {
		out = append(out, netip.MustParsePrefix(text))
	}
	return out
}

func TestTheDockerRangeIsTheTopBlockOfTheVpcWhenNoSubnetCoversIt(t *testing.T) {
	got, err := dockerAddressRange(netip.MustParsePrefix("10.0.0.0/16"), prefixes("10.0.0.0/19", "10.0.32.0/19"))

	if err != nil || got != netip.MustParsePrefix("10.0.255.224/27") {
		t.Fatalf("got %s, %v; want 10.0.255.224/27", got, err)
	}
}

func TestTheDockerRangeSkipsBlocksASubnetCovers(t *testing.T) {
	got, err := dockerAddressRange(netip.MustParsePrefix("10.0.0.0/24"), prefixes("10.0.0.128/25", "10.0.0.96/27"))

	if err != nil || got != netip.MustParsePrefix("10.0.0.64/27") {
		t.Fatalf("got %s, %v; want 10.0.0.64/27", got, err)
	}
}

func TestAVpcWhoseSubnetsCoverEveryBlockIsRefused(t *testing.T) {
	_, err := dockerAddressRange(netip.MustParsePrefix("10.0.0.0/24"), prefixes("10.0.0.0/24"))

	if err == nil {
		t.Fatal("a VPC with no free block was accepted")
	}
}

func TestTheGatewayIsTheFirstHostAddressOfTheVpc(t *testing.T) {
	if got := gatewayOf(netip.MustParsePrefix("10.20.0.0/16")); got != netip.MustParseAddr("10.20.0.1") {
		t.Fatalf("got %s", got)
	}
}
