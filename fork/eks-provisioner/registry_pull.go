// SPDX-License-Identifier: CC0-1.0

package main

import (
	"context"
	"crypto/x509"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"os"
	"path"
	"slices"
	"strings"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/ecr"
	ecrtypes "github.com/aws/aws-sdk-go-v2/service/ecr/types"
)

// How a node pulls from the emulator's ECR, as an Amazon EKS node pulls from ECR with its
// node role and no imagePullSecret: containerd trusts the emulator's CA for the registry's
// host, and the kubelet gets the registry's credentials from an image credential provider
// named ecr-credential-provider, in the directory the Amazon EKS AMIs keep it in. The
// provider is a script that hands the kubelet's request to the provisioner, which proves
// the caller is a kubelet of the cluster by its client certificate and answers with an
// authorization token GetAuthorizationToken just issued.
const (
	credentialProviderDir        = "/etc/eks/image-credential-provider"
	credentialProviderName       = "ecr-credential-provider"
	credentialProviderConfigPath = credentialProviderDir + "/config.yaml"
	credentialProviderCAPath     = credentialProviderDir + "/provisioner-ca.crt"
	// kubeletClientCertificate is the kubelet's client certificate and key, which
	// kubeadm's TLS bootstrap writes and the kubelet rotates; the cluster's CA signs it.
	kubeletClientCertificate = "/var/lib/kubelet/pki/kubelet-client-current.pem"
	// kubeletFlagsFile is where kubeadm join writes the kubelet's flags.
	kubeletFlagsFile = "/var/lib/kubelet/kubeadm-flags.env"
	// containerdCertsDir is the registry host configuration directory containerd 2 reads
	// by default (config_path of io.containerd.cri.v1.images in kind's node images).
	containerdCertsDir = "/etc/containerd/certs.d"
	// registryCredentialCacheDuration is how long the kubelet keeps a token before it asks
	// again. A token lives 12 hours; a short cache also recovers soon from an emulator
	// restart, which forgets every token it issued.
	registryCredentialCacheDuration = 10 * time.Minute
	// tokenExpiryMargin is how long before its expiry a token stops being handed out, so
	// a pull that starts with it does not outlive it.
	tokenExpiryMargin = time.Minute
	// credentialProviderAPIVersion is the credential provider API the kubelets of
	// Kubernetes 1.26 and later speak.
	credentialProviderAPIVersion = "credentialprovider.kubelet.k8s.io/v1"
)

// credentialProviderPlugin is the kubelet's image credential provider on a node. The
// kubelet runs it with a CredentialProviderRequest on stdin and the provider's env, and
// reads the CredentialProviderResponse from stdout.
const credentialProviderPlugin = `#!/bin/sh
# Image credential provider for the aws-emulator's ECR, installed by eks-provisioner. It sends
# the kubelet's CredentialProviderRequest to the provisioner, authenticated by the kubelet's
# client certificate, and prints the CredentialProviderResponse it answers with.
set -eu
exec curl --silent --show-error --fail --max-time 30 \
  --cacert "$REGISTRY_CREDENTIALS_CA" --cert "$KUBELET_CLIENT_CERTIFICATE" \
  --header 'Content-Type: application/json' --data-binary @- "$REGISTRY_CREDENTIALS_URL"
`

// registryPulls is what the nodes of one cluster need to pull from the emulator's ECR.
type registryPulls struct {
	// RegistryHost is the registry's host:port, the host of AWS_ENDPOINT_URL: images are
	// referenced as <RegistryHost>/<repository>.
	RegistryHost string
	EmulatorCA   []byte
	// CredentialsURL is where the provider asks the provisioner for a token.
	CredentialsURL string
	// ProvisionerCA signs the provisioner's serving certificate.
	ProvisionerCA []byte
}

// nodeFile is a file written on a node.
type nodeFile struct {
	Path    string
	Mode    string
	Content []byte
}

func (file nodeFile) String() string { return file.Path }

// files are what a node gets: the registry's host configuration for containerd, and the
// kubelet's image credential provider with its configuration.
func (pulls registryPulls) files() []nodeFile {
	hostDir := containerdCertsDir + "/" + pulls.RegistryHost
	server := "https://" + pulls.RegistryHost
	hosts := fmt.Sprintf(`# The aws-emulator's ECR, configured by eks-provisioner.
server = %q

[host.%q]
  capabilities = ["pull", "resolve"]
  ca = %q
`, server, server, hostDir+"/ca.crt")
	config := fmt.Sprintf(`apiVersion: kubelet.config.k8s.io/v1
kind: CredentialProviderConfig
providers:
- name: %s
  apiVersion: %s
  matchImages:
  - %q
  defaultCacheDuration: %q
  env:
  - {name: REGISTRY_CREDENTIALS_URL, value: %q}
  - {name: REGISTRY_CREDENTIALS_CA, value: %q}
  - {name: KUBELET_CLIENT_CERTIFICATE, value: %q}
`, credentialProviderName, credentialProviderAPIVersion, pulls.RegistryHost, registryCredentialCacheDuration.String(),
		pulls.CredentialsURL, credentialProviderCAPath, kubeletClientCertificate)
	return []nodeFile{
		{Path: hostDir + "/ca.crt", Mode: "0644", Content: pulls.EmulatorCA},
		{Path: hostDir + "/hosts.toml", Mode: "0644", Content: []byte(hosts)},
		{Path: credentialProviderCAPath, Mode: "0644", Content: pulls.ProvisionerCA},
		{Path: credentialProviderConfigPath, Mode: "0644", Content: []byte(config)},
		{Path: credentialProviderDir + "/" + credentialProviderName, Mode: "0755", Content: []byte(credentialProviderPlugin)},
	}
}

// credentialProviderFlags are the kubelet flags that turn the provider on, which the
// kubelet reads only when it starts.
var credentialProviderFlags = []string{
	"--image-credential-provider-config=" + credentialProviderConfigPath,
	"--image-credential-provider-bin-dir=" + credentialProviderDir,
}

// enableCredentialProvider adds the provider's flags to the kubelet flags kubeadm join
// wrote, for a node that joined before nodes got them, and says whether it changed them.
func enableCredentialProvider(flags string) (string, bool, error) {
	if strings.Contains(flags, credentialProviderFlags[0]) {
		return flags, false, nil
	}
	const variable = `KUBELET_KUBEADM_ARGS="`
	if !strings.Contains(flags, variable) {
		return "", false, fmt.Errorf("%s sets no KUBELET_KUBEADM_ARGS: %q", kubeletFlagsFile, flags)
	}
	return strings.Replace(flags, variable, variable+strings.Join(credentialProviderFlags, " ")+" ", 1), true, nil
}

// providerAuth is one registry's credentials in a CredentialProviderResponse.
type providerAuth struct {
	Username string `json:"username"`
	Password string `json:"password"`
}

// providerResponse is the credentialprovider.kubelet.k8s.io/v1 CredentialProviderResponse.
type providerResponse struct {
	APIVersion    string                  `json:"apiVersion"`
	Kind          string                  `json:"kind"`
	CacheKeyType  string                  `json:"cacheKeyType"`
	CacheDuration string                  `json:"cacheDuration"`
	Auth          map[string]providerAuth `json:"auth"`
}

// providerRequest is the part of a CredentialProviderRequest the provisioner reads.
type providerRequest struct {
	Image string `json:"image"`
}

// credentialProviderResponse is the kubelet's answer for an ECR authorization token that
// expires at expiresAt: the token's user and password for the registry host, cached no
// longer than registryCredentialCacheDuration nor past the token's expiry.
func credentialProviderResponse(registryHost, token string, expiresAt, now time.Time) (providerResponse, error) {
	decoded, err := base64.StdEncoding.DecodeString(token)
	if err != nil {
		return providerResponse{}, fmt.Errorf("the ECR authorization token is not Base64: %w", err)
	}
	username, password, ok := strings.Cut(string(decoded), ":")
	if !ok || username == "" || password == "" {
		return providerResponse{}, errors.New("the ECR authorization token does not decode to user:password")
	}
	lifetime := expiresAt.Sub(now) - tokenExpiryMargin
	if lifetime <= 0 {
		return providerResponse{}, fmt.Errorf("the ECR authorization token expires at %s, too soon to hand out", expiresAt.UTC().Format(time.RFC3339))
	}
	return providerResponse{
		APIVersion:    credentialProviderAPIVersion,
		Kind:          "CredentialProviderResponse",
		CacheKeyType:  "Registry",
		CacheDuration: min(lifetime, registryCredentialCacheDuration).Truncate(time.Second).String(),
		Auth:          map[string]providerAuth{registryHost: {Username: username, Password: password}},
	}, nil
}

// nodeIdentity is the node name a client certificate chain authenticates in a cluster:
// a kubelet's client certificate, signed by the cluster's CA, of user system:node:<name>
// in group system:nodes.
func nodeIdentity(chain []*x509.Certificate, clusterCA []byte) (string, error) {
	if len(chain) == 0 {
		return "", errors.New("no client certificate was presented")
	}
	roots := x509.NewCertPool()
	if !roots.AppendCertsFromPEM(clusterCA) {
		return "", errors.New("the cluster's CA holds no certificate")
	}
	intermediates := x509.NewCertPool()
	for _, cert := range chain[1:] {
		intermediates.AddCert(cert)
	}
	leaf := chain[0]
	if _, err := leaf.Verify(x509.VerifyOptions{Roots: roots, Intermediates: intermediates, KeyUsages: []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth}}); err != nil {
		return "", fmt.Errorf("the client certificate of %q is not the cluster's: %w", leaf.Subject.CommonName, err)
	}
	name, ok := strings.CutPrefix(leaf.Subject.CommonName, "system:node:")
	if !ok || name == "" || !slices.Contains(leaf.Subject.Organization, "system:nodes") {
		return "", fmt.Errorf("%q is not a node of the cluster", leaf.Subject.CommonName)
	}
	return name, nil
}

// imageOnRegistry is whether an image reference names a repository of registryHost.
func imageOnRegistry(image, registryHost string) bool {
	host, _, found := strings.Cut(image, "/")
	return found && strings.EqualFold(host, registryHost)
}

// serveRegistryCredentials answers POST /registry-credentials/{region}/{cluster}, which a
// node's credential provider calls with the kubelet's CredentialProviderRequest.
func (p *provisioner) serveRegistryCredentials(writer http.ResponseWriter, request *http.Request) {
	region, cluster, ok := strings.Cut(strings.TrimPrefix(request.URL.Path, "/registry-credentials/"), "/")
	if request.Method != http.MethodPost || !ok || region == "" || cluster == "" || strings.Contains(cluster, "/") {
		http.Error(writer, "POST /registry-credentials/{region}/{cluster}", http.StatusNotFound)
		return
	}
	var chain []*x509.Certificate
	if request.TLS != nil {
		chain = request.TLS.PeerCertificates
	}
	clusterCA, err := os.ReadFile(clusterSpec{StateDir: p.clusterDir(region, cluster)}.caCert())
	if err != nil {
		p.log.Info("registry credentials refused", "cluster", cluster, "region", region, "reason", "the cluster is not this provisioner's")
		http.Error(writer, "cluster "+cluster+" is not provisioned here", http.StatusForbidden)
		return
	}
	node, err := nodeIdentity(chain, clusterCA)
	if err != nil {
		p.log.Info("registry credentials refused", "cluster", cluster, "region", region, "reason", err.Error())
		http.Error(writer, err.Error(), http.StatusForbidden)
		return
	}
	var asked providerRequest
	if err := json.NewDecoder(http.MaxBytesReader(writer, request.Body, 64<<10)).Decode(&asked); err != nil {
		http.Error(writer, "the body is not a CredentialProviderRequest: "+err.Error(), http.StatusBadRequest)
		return
	}
	if !imageOnRegistry(asked.Image, p.registryHost) {
		http.Error(writer, fmt.Sprintf("image %q is not in the emulator's registry %s", asked.Image, p.registryHost), http.StatusBadRequest)
		return
	}
	token, err := p.registryToken(request.Context(), region)
	if err != nil {
		p.log.Error("issuing registry credentials", "cluster", cluster, "region", region, "node", node, "error", err)
		http.Error(writer, err.Error(), http.StatusBadGateway)
		return
	}
	response, err := credentialProviderResponse(p.registryHost, aws.ToString(token.AuthorizationToken), aws.ToTime(token.ExpiresAt), time.Now())
	if err != nil {
		p.log.Error("issuing registry credentials", "cluster", cluster, "region", region, "node", node, "error", err)
		http.Error(writer, err.Error(), http.StatusBadGateway)
		return
	}
	p.log.Info("registry credentials issued", "cluster", cluster, "region", region, "node", node, "image", asked.Image, "cache", response.CacheDuration)
	writer.Header().Set("Content-Type", "application/json")
	if err := json.NewEncoder(writer).Encode(response); err != nil {
		p.log.Error("answering a credential provider", "cluster", cluster, "node", node, "error", err)
	}
}

// registryToken is an authorization token the emulator's ECR issues now, to the
// provisioner's principal.
func (p *provisioner) registryToken(ctx context.Context, region string) (*ecrtypes.AuthorizationData, error) {
	client := ecr.NewFromConfig(p.awsConfig, func(options *ecr.Options) { options.Region = region })
	issued, err := client.GetAuthorizationToken(ctx, &ecr.GetAuthorizationTokenInput{})
	if err != nil {
		return nil, fmt.Errorf("ECR GetAuthorizationToken in %s: %w", region, err)
	}
	if len(issued.AuthorizationData) == 0 || issued.AuthorizationData[0].AuthorizationToken == nil || issued.AuthorizationData[0].ExpiresAt == nil {
		return nil, fmt.Errorf("ECR GetAuthorizationToken in %s answered no token", region)
	}
	return &issued.AuthorizationData[0], nil
}

// configureImagePulls writes the registry's host configuration and the credential
// provider on a node container. Writing them again is harmless: containerd reads a host's
// configuration at each pull, and the kubelet reads the provider at each pull too.
func configureImagePulls(ctx context.Context, container string, pulls registryPulls) error {
	for _, file := range pulls.files() {
		if err := dockerWithInput(ctx, string(file.Content), "exec", "-i", container, "sh", "-c",
			`mkdir -p "$(dirname "$1")" && cat > "$1.tmp" && chmod "$2" "$1.tmp" && mv "$1.tmp" "$1"`, "sh", file.Path, file.Mode); err != nil {
			return fmt.Errorf("writing %s on %s: %w", file.Path, container, err)
		}
	}
	return nil
}

// enableCredentialProviderOn turns the provider on for a node that joined without its
// flags, and restarts its kubelet, which leaves its pods running.
func enableCredentialProviderOn(ctx context.Context, container string) error {
	flags, err := docker(ctx, "exec", container, "cat", kubeletFlagsFile)
	if err != nil {
		return err
	}
	enabled, changed, err := enableCredentialProvider(flags + "\n")
	if err != nil || !changed {
		return err
	}
	if err := dockerWithInput(ctx, enabled, "exec", "-i", container, "sh", "-c", `cat > "$1"`, "sh", kubeletFlagsFile); err != nil {
		return err
	}
	_, err = docker(ctx, "exec", container, "systemctl", "restart", "kubelet")
	return err
}

// nodePulls is the registryPulls of a node container: the provider reaches the
// provisioner at the gateway of the node's network, as the API servers do.
func (p *provisioner) nodePulls(ctx context.Context, region, name, container string) (registryPulls, error) {
	gateways, err := docker(ctx, "inspect", "-f", "{{range .NetworkSettings.Networks}}{{.Gateway}} {{end}}", container)
	if err != nil {
		return registryPulls{}, err
	}
	fields := strings.Fields(gateways)
	if len(fields) == 0 {
		return registryPulls{}, fmt.Errorf("node %s has no network gateway", container)
	}
	return registryPulls{
		RegistryHost:   p.registryHost,
		EmulatorCA:     p.trust.EmulatorCA,
		CredentialsURL: fmt.Sprintf("https://%s:%s/%s", fields[0], p.listenPort, path.Join("registry-credentials", region, name)),
		ProvisionerCA:  p.webhook.caPEM,
	}, nil
}

// ensureImagePulls configures a node that joined earlier, once per provisioner run.
func (p *provisioner) ensureImagePulls(ctx context.Context, region, name, container string) error {
	if p.pullsConfigured[container] {
		return nil
	}
	pulls, err := p.nodePulls(ctx, region, name, container)
	if err != nil {
		return err
	}
	if err := configureImagePulls(ctx, container, pulls); err != nil {
		return err
	}
	if err := enableCredentialProviderOn(ctx, container); err != nil {
		return fmt.Errorf("turning the image credential provider on for %s: %w", container, err)
	}
	p.pullsConfigured[container] = true
	return nil
}
