// SPDX-License-Identifier: CC0-1.0

package main

import (
	"encoding/base64"
	"strings"
	"testing"
	"time"
)

const testSTSHost = "172.29.0.1:9597"

var signedAt = time.Date(2026, 11, 21, 12, 0, 0, 0, time.UTC)

func token(url string) string {
	return tokenPrefix + base64.RawURLEncoding.EncodeToString([]byte(url))
}

func presignedURL(host, signedHeaders string) string {
	return "https://" + host + "/?Action=GetCallerIdentity&Version=2011-06-15&X-Amz-Algorithm=AWS4-HMAC-SHA256" +
		"&X-Amz-Credential=AKID%2F20261121%2Fus-east-1%2Fsts%2Faws4_request&X-Amz-Date=20261121T120000Z" +
		"&X-Amz-Expires=60&X-Amz-SignedHeaders=" + signedHeaders + "&X-Amz-Signature=abc"
}

func TestATokenForTheEmulatorsSTSIsDecodedIntoItsRequest(t *testing.T) {
	parsed, err := parseToken(token(presignedURL(testSTSHost, "host%3Bx-k8s-aws-id")), testSTSHost, signedAt.Add(30*time.Second))

	if err != nil || parsed.URL.Host != testSTSHost {
		t.Fatalf("got %+v, %v", parsed, err)
	}
}

func TestATokenWhoseRequestGoesElsewhereIsRefused(t *testing.T) {
	_, err := parseToken(token(presignedURL("evil.example", "host%3Bx-k8s-aws-id")), testSTSHost, signedAt)

	if err == nil || !strings.Contains(err.Error(), "evil.example") {
		t.Fatalf("got %v", err)
	}
}

func TestATokenThatSignsNoClusterIsRefused(t *testing.T) {
	_, err := parseToken(token(presignedURL(testSTSHost, "host")), testSTSHost, signedAt)

	if err == nil || !strings.Contains(err.Error(), clusterIDHeader) {
		t.Fatalf("got %v", err)
	}
}

func TestAnExpiredTokenIsRefused(t *testing.T) {
	_, err := parseToken(token(presignedURL(testSTSHost, "host%3Bx-k8s-aws-id")), testSTSHost, signedAt.Add(2*time.Minute))

	if err == nil || !strings.Contains(err.Error(), "expired") {
		t.Fatalf("got %v", err)
	}
}

func TestATokenWithoutThePrefixIsRefused(t *testing.T) {
	if _, err := parseToken("eyJhbGciOi", testSTSHost, signedAt); err == nil {
		t.Fatal("a token without the prefix was accepted")
	}
}

func TestAnAssumedRoleSessionMapsToItsRole(t *testing.T) {
	got, err := principalARN("arn:aws:sts::000000000000:assumed-role/terraform/session-1")

	if err != nil || got != "arn:aws:iam::000000000000:role/terraform" {
		t.Fatalf("got %q, %v", got, err)
	}
}

func TestAUserIsItsOwnPrincipal(t *testing.T) {
	got, err := principalARN("arn:aws:iam::000000000000:user/ci")

	if err != nil || got != "arn:aws:iam::000000000000:user/ci" {
		t.Fatalf("got %q, %v", got, err)
	}
}

func TestAnArnOfAnotherServiceIsRefused(t *testing.T) {
	if _, err := principalARN("arn:aws:s3:::bucket"); err == nil {
		t.Fatal("an S3 ARN was accepted")
	}
}

func TestTheUsernameTemplateIsFilledFromTheSession(t *testing.T) {
	got := expandUsername("admin:{{AccountID}}:{{SessionName}}", "arn:aws:sts::000000000000:assumed-role/ops/paulo@example", "000000000000")

	if got != "admin:000000000000:paulo-example" {
		t.Fatalf("got %q", got)
	}
}

func TestAClusterScopedAdminPolicyMapsToItsGroup(t *testing.T) {
	group, policy, err := policyGroup("arn:aws:eks::aws:cluster-access-policy/AmazonEKSClusterAdminPolicy", "")

	if err != nil || group != "eks-access-policy:AmazonEKSClusterAdminPolicy" || policy != "AmazonEKSClusterAdminPolicy" {
		t.Fatalf("got %q %q %v", group, policy, err)
	}
}

func TestANamespaceScopedPolicyMapsToAGroupPerNamespace(t *testing.T) {
	group, _, err := policyGroup("arn:aws:eks::aws:cluster-access-policy/AmazonEKSEditPolicy", "tenants")

	if err != nil || group != "eks-access-policy:AmazonEKSEditPolicy:tenants" {
		t.Fatalf("got %q %v", group, err)
	}
}

func TestAPolicyWithoutAClusterRoleIsRefused(t *testing.T) {
	if _, _, err := policyGroup("arn:aws:eks::aws:cluster-access-policy/AmazonEKSNetworkingPolicy", ""); err == nil {
		t.Fatal("an unmapped policy was accepted")
	}
}

func TestAnEKSVersionWithoutANodeImageIsRefusedWithTheSupportedOnes(t *testing.T) {
	_, err := nodeImage("1.36")

	if err == nil || !strings.Contains(err.Error(), "1.35") {
		t.Fatalf("got %v", err)
	}
}
