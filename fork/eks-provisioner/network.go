// SPDX-License-Identifier: CC0-1.0

package main

import (
	"fmt"
	"net/netip"
)

// dockerRangeBits is the size of the block of a VPC's CIDR that Docker hands out
// addresses from: the control-plane containers, which live outside the VPC's subnets
// the way the hosts of an Amazon EKS control plane live outside the customer's VPC.
const dockerRangeBits = 27

// dockerAddressRange is the highest /27 of vpc that no subnet of the VPC overlaps.
//
// Docker assigns a container that names no address one from this block, and EC2 assigns
// an instance or a network interface one from its subnet, so the two never hand out the
// same address as long as no subnet is created over the block later. The block is taken
// from the top of the CIDR because EC2 fills a subnet from its bottom.
func dockerAddressRange(vpc netip.Prefix, subnets []netip.Prefix) (netip.Prefix, error) {
	vpc = vpc.Masked()
	if !vpc.Addr().Is4() {
		return netip.Prefix{}, fmt.Errorf("VPC CIDR %s is not IPv4", vpc)
	}
	if vpc.Bits() > dockerRangeBits-1 {
		return netip.Prefix{}, fmt.Errorf("VPC CIDR %s is smaller than two /%d blocks; it leaves no room for the control plane", vpc, dockerRangeBits)
	}
	blocks := 1 << (dockerRangeBits - vpc.Bits())
	base := addrToUint(vpc.Addr())
	for index := blocks - 1; index >= 1; index-- {
		candidate := netip.PrefixFrom(uintToAddr(base+uint32(index)<<(32-dockerRangeBits)), dockerRangeBits)
		if !overlapsAny(candidate, subnets) {
			return candidate, nil
		}
	}
	return netip.Prefix{}, fmt.Errorf("every /%d of VPC CIDR %s above its first is covered by a subnet; leave one free for the control plane", dockerRangeBits, vpc)
}

// gatewayOf is the first host address of vpc: the address Docker gives the host on the
// VPC's bridge, which is the address the VPC router holds on AWS.
func gatewayOf(vpc netip.Prefix) netip.Addr {
	return vpc.Masked().Addr().Next()
}

func overlapsAny(prefix netip.Prefix, others []netip.Prefix) bool {
	for _, other := range others {
		if prefix.Overlaps(other) {
			return true
		}
	}
	return false
}

func addrToUint(addr netip.Addr) uint32 {
	bytes := addr.As4()
	return uint32(bytes[0])<<24 | uint32(bytes[1])<<16 | uint32(bytes[2])<<8 | uint32(bytes[3])
}

func uintToAddr(value uint32) netip.Addr {
	return netip.AddrFrom4([4]byte{byte(value >> 24), byte(value >> 16), byte(value >> 8), byte(value)})
}
