// SPDX-License-Identifier: CC0-1.0

package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"encoding/hex"
	"encoding/json"
	"encoding/xml"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	v4 "github.com/aws/aws-sdk-go-v2/aws/signer/v4"
)

// emulator calls the fork's own routes of the emulator, signed with SigV4 by the
// emulator's keys, and its STS for the identity a token carries.
type emulator struct {
	endpoint *url.URL
	// stsHosts are the names a token's request may go to: the endpoint's host, and the
	// other names clients reach the same emulator by.
	stsHosts []string
	creds    aws.CredentialsProvider
	client   *http.Client
	signer   *v4.Signer
}

func newEmulator(endpoint string, aliases []string, caBundle string, creds aws.CredentialsProvider) (*emulator, error) {
	parsed, err := url.Parse(endpoint)
	if err != nil || parsed.Scheme != "https" || parsed.Host == "" {
		return nil, fmt.Errorf("AWS_ENDPOINT_URL %q is not an https URL", endpoint)
	}
	pool, err := x509.SystemCertPool()
	if err != nil {
		pool = x509.NewCertPool()
	}
	if caBundle != "" {
		pem, err := os.ReadFile(caBundle)
		if err != nil {
			return nil, fmt.Errorf("reading AWS_CA_BUNDLE: %w", err)
		}
		if !pool.AppendCertsFromPEM(pem) {
			return nil, fmt.Errorf("AWS_CA_BUNDLE %s holds no PEM certificate", caBundle)
		}
	}
	return &emulator{
		endpoint: parsed,
		stsHosts: append([]string{parsed.Host}, aliases...),
		creds:    creds,
		client:   &http.Client{Timeout: 30 * time.Second, Transport: &http.Transport{TLSClientConfig: &tls.Config{RootCAs: pool, MinVersion: tls.VersionTLS12}}},
		signer:   v4.NewSigner(),
	}, nil
}

// controlPlane is what GET /_fork/eks/clusters/{name}/control-plane answers.
type controlPlane struct {
	Name                 string  `json:"name"`
	Status               string  `json:"status"`
	Version              string  `json:"version"`
	Issuer               string  `json:"issuer"`
	Endpoint             *string `json:"endpoint"`
	CertificateAuthority struct {
		ID         string `json:"id"`
		Data       string `json:"data"`
		SigningKey string `json:"signingKey"`
	} `json:"certificateAuthority"`
}

// String never prints the signing key.
func (plane controlPlane) String() string {
	return fmt.Sprintf("control plane of %s (%s, %s)", plane.Name, plane.Status, plane.Version)
}

func (e *emulator) controlPlane(ctx context.Context, region, cluster string) (controlPlane, error) {
	var plane controlPlane
	body, err := e.fork(ctx, http.MethodGet, region, cluster, "control-plane", nil)
	if err != nil {
		return plane, err
	}
	if err := json.Unmarshal(body, &plane); err != nil {
		return plane, fmt.Errorf("the control plane of %s is not JSON: %w", cluster, err)
	}
	return plane, nil
}

func (e *emulator) registerEndpoint(ctx context.Context, region, cluster, endpoint string) error {
	payload, _ := json.Marshal(map[string]string{"endpoint": endpoint})
	_, err := e.fork(ctx, http.MethodPut, region, cluster, "control-plane", payload)
	return err
}

func (e *emulator) registerKeySet(ctx context.Context, region, cluster string, jwks []byte) error {
	_, err := e.fork(ctx, http.MethodPut, region, cluster, "service-account-issuer-keys", jwks)
	return err
}

func (e *emulator) fork(ctx context.Context, method, region, cluster, segment string, payload []byte) ([]byte, error) {
	target := e.endpoint.JoinPath("_fork", "eks", "clusters", cluster, segment)
	request, err := http.NewRequestWithContext(ctx, method, target.String(), bytes.NewReader(payload))
	if err != nil {
		return nil, err
	}
	request.Header.Set("Content-Type", "application/json")
	creds, err := e.creds.Retrieve(ctx)
	if err != nil {
		return nil, fmt.Errorf("the emulator's credentials: %w", err)
	}
	digest := sha256.Sum256(payload)
	if err := e.signer.SignHTTP(ctx, creds, request, hex.EncodeToString(digest[:]), "eks", region, time.Now()); err != nil {
		return nil, fmt.Errorf("signing %s %s: %w", method, target.Path, err)
	}
	response, err := e.client.Do(request)
	if err != nil {
		return nil, fmt.Errorf("%s %s: %w", method, target.Path, err)
	}
	defer response.Body.Close()
	body, err := io.ReadAll(io.LimitReader(response.Body, 1<<20))
	if err != nil {
		return nil, fmt.Errorf("%s %s: reading the answer: %w", method, target.Path, err)
	}
	if response.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("%s %s answered %d: %s", method, target.Path, response.StatusCode, body)
	}
	return body, nil
}

// callerIdentity is the STS GetCallerIdentity result a token's presigned request yields.
type callerIdentity struct {
	Arn     string `xml:"GetCallerIdentityResult>Arn"`
	Account string `xml:"GetCallerIdentityResult>Account"`
	UserID  string `xml:"GetCallerIdentityResult>UserId"`
}

// callerOf sends a token's presigned request to the emulator's STS, as Amazon EKS sends it
// to STS, with the cluster name in the signed header. STS answers only if the signature
// is good, which is the whole of the token's authentication.
func (e *emulator) callerOf(ctx context.Context, identity presignedIdentity, cluster string) (callerIdentity, error) {
	var caller callerIdentity
	// The request goes to the endpoint whatever name the token was signed for, and keeps
	// that name as its Host, which the signature covers: the token chooses a name, never
	// where the provisioner connects.
	target := *identity.URL
	target.Scheme, target.Host = e.endpoint.Scheme, e.endpoint.Host
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, target.String(), nil)
	if err != nil {
		return caller, err
	}
	request.Host = identity.URL.Host
	request.Header.Set(clusterIDHeader, cluster)
	response, err := e.client.Do(request)
	if err != nil {
		return caller, fmt.Errorf("STS GetCallerIdentity: %w", err)
	}
	defer response.Body.Close()
	body, err := io.ReadAll(io.LimitReader(response.Body, 1<<16))
	if err != nil {
		return caller, fmt.Errorf("STS GetCallerIdentity: reading the answer: %w", err)
	}
	if response.StatusCode != http.StatusOK {
		return caller, fmt.Errorf("STS refused the token's request with %d: %s", response.StatusCode, body)
	}
	if err := xml.Unmarshal(body, &caller); err != nil || caller.Arn == "" {
		return caller, fmt.Errorf("STS GetCallerIdentity answered no ARN: %s", body)
	}
	return caller, nil
}
