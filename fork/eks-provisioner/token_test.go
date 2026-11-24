// SPDX-License-Identifier: CC0-1.0

package main

import (
	"context"
	"encoding/base64"
	"net/http"
	"net/http/httptest"
	"net/url"
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
	parsed, err := parseToken(token(presignedURL(testSTSHost, "host%3Bx-k8s-aws-id")), []string{testSTSHost}, signedAt.Add(30*time.Second))

	if err != nil || parsed.URL.Host != testSTSHost {
		t.Fatalf("got %+v, %v", parsed, err)
	}
}

func TestATokenWhoseRequestGoesElsewhereIsRefused(t *testing.T) {
	_, err := parseToken(token(presignedURL("evil.example", "host%3Bx-k8s-aws-id")), []string{testSTSHost}, signedAt)

	if err == nil || !strings.Contains(err.Error(), "evil.example") {
		t.Fatalf("got %v", err)
	}
}

func TestATokenThatSignsNoClusterIsRefused(t *testing.T) {
	_, err := parseToken(token(presignedURL(testSTSHost, "host")), []string{testSTSHost}, signedAt)

	if err == nil || !strings.Contains(err.Error(), clusterIDHeader) {
		t.Fatalf("got %v", err)
	}
}

func TestATokenIsAcceptedForFifteenMinutesWhateverItsPresignExpiry(t *testing.T) {
	// aws eks get-token presigns for 60 seconds and kubectl keeps the token for 14 minutes:
	// Amazon EKS accepts it for 15 minutes from X-Amz-Date, as aws-iam-authenticator does.
	_, err := parseToken(token(presignedURL(testSTSHost, "host%3Bx-k8s-aws-id")), []string{testSTSHost}, signedAt.Add(14*time.Minute))

	if err != nil {
		t.Fatalf("a token signed 14 minutes ago was refused: %v", err)
	}
}

func TestATokenOlderThanFifteenMinutesIsRefused(t *testing.T) {
	_, err := parseToken(token(presignedURL(testSTSHost, "host%3Bx-k8s-aws-id")), []string{testSTSHost}, signedAt.Add(16*time.Minute))

	if err == nil || !strings.Contains(err.Error(), "more than 15m0s") {
		t.Fatalf("got %v", err)
	}
}

func TestATokenWithoutThePrefixIsRefused(t *testing.T) {
	if _, err := parseToken("eyJhbGciOi", []string{testSTSHost}, signedAt); err == nil {
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

func TestATokenForAnotherNameOfTheEmulatorIsAccepted(t *testing.T) {
	// The host reaches the emulator at localhost, and its VPCs at the gateway: both are it.
	hosts := []string{testSTSHost, "localhost:9597"}

	parsed, err := parseToken(token(presignedURL("localhost:9597", "host%3Bx-k8s-aws-id")), hosts, signedAt)

	if err != nil || parsed.URL.Host != "localhost:9597" {
		t.Fatalf("got %+v, %v", parsed, err)
	}
}

func TestATokensRequestIsSentToTheEmulatorUnderTheHostItWasSignedFor(t *testing.T) {
	var seenHost, seenCluster string
	server := httptest.NewTLSServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		seenHost, seenCluster = request.Host, request.Header.Get(clusterIDHeader)
		writer.Write([]byte(`<GetCallerIdentityResponse><GetCallerIdentityResult><Arn>arn:aws:iam::000000000000:user/emulator</Arn><UserId>AIDA</UserId><Account>000000000000</Account></GetCallerIdentityResult></GetCallerIdentityResponse>`))
	}))
	defer server.Close()
	endpoint, _ := url.Parse(server.URL)
	emu := &emulator{endpoint: endpoint, client: server.Client()}
	presigned, _ := url.Parse(presignedURL("localhost:9597", "host%3Bx-k8s-aws-id"))

	caller, err := emu.callerOf(context.Background(), presignedIdentity{URL: presigned}, "core")

	if err != nil || caller.Arn != "arn:aws:iam::000000000000:user/emulator" {
		t.Fatalf("got %+v, %v", caller, err)
	}
	if seenHost != "localhost:9597" || seenCluster != "core" {
		t.Fatalf("the emulator saw Host %q and cluster %q", seenHost, seenCluster)
	}
}

func TestTheWebhookStoppingOnShutdownIsNoError(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	if err := webhookStopped(ctx, nil); err != nil {
		t.Fatalf("got %v", err)
	}
}

func TestTheWebhookStoppingOnItsOwnIsAnErrorThatSaysSo(t *testing.T) {
	err := webhookStopped(context.Background(), nil)

	if err == nil || strings.Contains(err.Error(), "%!") || !strings.Contains(err.Error(), "token webhook stopped") {
		t.Fatalf("got %v", err)
	}
}

func TestATokenSignedForTheNamePodsReachTheEmulatorByIsAccepted(t *testing.T) {
	// A pod's AWS_ENDPOINT_URL_STS names the emulator as aws.emulator.internal, so a token
	// `aws eks get-token` makes in a pod is presigned for that host.
	emu, err := newEmulator("https://172.29.0.1:9597", nil, "", nil)
	if err != nil {
		t.Fatal(err)
	}

	_, err = parseToken(token(presignedURL("aws.emulator.internal:9597", "host%3Bx-k8s-aws-id")), emu.stsHosts, signedAt)

	if err != nil {
		t.Fatalf("a token signed for the pods' name of the emulator was refused: %v", err)
	}
}
