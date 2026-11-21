// SPDX-License-Identifier: CC0-1.0

package main

import (
	"encoding/base64"
	"fmt"
	"net/url"
	"strconv"
	"strings"
	"time"
)

// tokenPrefix starts every token `aws eks get-token` prints.
const tokenPrefix = "k8s-aws-v1."

// clusterIDHeader is the header the presigned request signs the cluster name into, so a
// token minted for one cluster is refused by every other.
const clusterIDHeader = "x-k8s-aws-id"

// maxTokenAge is how old a token's signature may be, the window aws-iam-authenticator
// and Amazon EKS accept.
const maxTokenAge = 15 * time.Minute

// presignedIdentity is a token decoded into the STS GetCallerIdentity request it carries.
type presignedIdentity struct {
	URL *url.URL
}

// parseToken decodes a bearer token into its presigned request and checks what can be
// checked without calling STS: the form, the action, that the cluster header is signed,
// the signing date, and that the request goes to the emulator (stsHost) and nowhere else.
func parseToken(token, stsHost string, now time.Time) (presignedIdentity, error) {
	encoded, ok := strings.CutPrefix(token, tokenPrefix)
	if !ok {
		return presignedIdentity{}, fmt.Errorf("the token does not start with %q", tokenPrefix)
	}
	raw, err := base64.RawURLEncoding.DecodeString(strings.TrimRight(encoded, "="))
	if err != nil {
		return presignedIdentity{}, fmt.Errorf("the token is not base64url: %w", err)
	}
	presigned, err := url.Parse(string(raw))
	if err != nil {
		return presignedIdentity{}, fmt.Errorf("the token does not hold a URL: %w", err)
	}
	if presigned.Scheme != "https" || presigned.Host != stsHost {
		return presignedIdentity{}, fmt.Errorf("the token's request goes to %s://%s, not to STS at https://%s", presigned.Scheme, presigned.Host, stsHost)
	}
	query := presigned.Query()
	if query.Get("Action") != "GetCallerIdentity" {
		return presignedIdentity{}, fmt.Errorf("the token's request is %q, not GetCallerIdentity", query.Get("Action"))
	}
	if query.Get("X-Amz-Algorithm") != "AWS4-HMAC-SHA256" {
		return presignedIdentity{}, fmt.Errorf("the token's request is not presigned with SigV4")
	}
	signed := strings.Split(strings.ToLower(query.Get("X-Amz-SignedHeaders")), ";")
	if !contains(signed, clusterIDHeader) {
		return presignedIdentity{}, fmt.Errorf("the token's request does not sign %s, so it names no cluster", clusterIDHeader)
	}
	signedAt, err := time.Parse("20060102T150405Z", query.Get("X-Amz-Date"))
	if err != nil {
		return presignedIdentity{}, fmt.Errorf("the token's X-Amz-Date %q is not a SigV4 date", query.Get("X-Amz-Date"))
	}
	if age := now.Sub(signedAt); age > maxTokenAge || age < -maxTokenAge {
		return presignedIdentity{}, fmt.Errorf("the token was signed at %s, more than %s from now", signedAt.Format(time.RFC3339), maxTokenAge)
	}
	if expires, err := strconv.Atoi(query.Get("X-Amz-Expires")); err == nil && now.After(signedAt.Add(time.Duration(expires)*time.Second)) {
		return presignedIdentity{}, fmt.Errorf("the token expired at %s", signedAt.Add(time.Duration(expires)*time.Second).Format(time.RFC3339))
	}
	return presignedIdentity{URL: presigned}, nil
}

// principalARN is the IAM principal an access entry names for a caller's ARN: an
// assumed-role session is its role, and every other ARN is itself.
func principalARN(callerARN string) (string, error) {
	parts := strings.SplitN(callerARN, ":", 6)
	if len(parts) != 6 || parts[0] != "arn" {
		return "", fmt.Errorf("%q is not an ARN", callerARN)
	}
	partition, service, account, resource := parts[1], parts[2], parts[4], parts[5]
	if service == "sts" {
		session, ok := strings.CutPrefix(resource, "assumed-role/")
		if !ok {
			return "", fmt.Errorf("%q is an STS ARN but not an assumed-role session", callerARN)
		}
		role, _, found := strings.Cut(session, "/")
		if !found || role == "" {
			return "", fmt.Errorf("%q names no role", callerARN)
		}
		return fmt.Sprintf("arn:%s:iam::%s:role/%s", partition, account, role), nil
	}
	if service != "iam" {
		return "", fmt.Errorf("%q is neither an IAM nor an STS ARN", callerARN)
	}
	return callerARN, nil
}

// sessionName is the session of an assumed-role ARN, empty for any other principal.
func sessionName(callerARN string) string {
	_, resource, _ := strings.Cut(callerARN, ":assumed-role/")
	_, session, _ := strings.Cut(resource, "/")
	return session
}

// expandUsername fills the templates an access entry's username may hold.
func expandUsername(template, callerARN, account string) string {
	session := sessionName(callerARN)
	return strings.NewReplacer(
		"{{SessionName}}", strings.ReplaceAll(session, "@", "-"),
		"{{SessionNameRaw}}", session,
		"{{AccountID}}", account,
	).Replace(template)
}

// policyClusterRoles maps the Amazon EKS access policies to the ClusterRole each one
// grants, the mapping the Amazon EKS user guide documents.
var policyClusterRoles = map[string]string{
	"AmazonEKSClusterAdminPolicy": "cluster-admin",
	"AmazonEKSAdminPolicy":        "admin",
	"AmazonEKSEditPolicy":         "edit",
	"AmazonEKSViewPolicy":         "view",
}

// policyGroup is the Kubernetes group an access policy association puts its principal in:
// cluster-wide for a cluster scope, one per namespace for a namespace scope. A binding of
// that group to the policy's ClusterRole is what grants the policy.
func policyGroup(policyARN, namespace string) (group, policy string, err error) {
	_, name, found := strings.Cut(policyARN, ":cluster-access-policy/")
	if !found {
		return "", "", fmt.Errorf("%q is not an Amazon EKS access policy ARN", policyARN)
	}
	if _, ok := policyClusterRoles[name]; !ok {
		return "", "", fmt.Errorf("access policy %s has no ClusterRole here", name)
	}
	if namespace == "" {
		return "eks-access-policy:" + name, name, nil
	}
	return "eks-access-policy:" + name + ":" + namespace, name, nil
}

func contains(values []string, wanted string) bool {
	for _, value := range values {
		if value == wanted {
			return true
		}
	}
	return false
}
