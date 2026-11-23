// SPDX-License-Identifier: CC0-1.0

package main

import (
	"bytes"
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/json"
	"encoding/pem"
	"io"
	"log/slog"
	"math/big"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"sigs.k8s.io/yaml"
)

const testRegistryHost = "172.29.0.1:9597"

func testPulls() registryPulls {
	return registryPulls{
		RegistryHost:   testRegistryHost,
		EmulatorCA:     []byte("-----BEGIN CERTIFICATE-----\nemulator\n-----END CERTIFICATE-----\n"),
		CredentialsURL: "https://10.0.0.1:9598/registry-credentials/us-east-1/core",
		ProvisionerCA:  []byte("-----BEGIN CERTIFICATE-----\nprovisioner\n-----END CERTIFICATE-----\n"),
	}
}

func fileAt(t *testing.T, files []nodeFile, path string) nodeFile {
	t.Helper()
	for _, file := range files {
		if file.Path == path {
			return file
		}
	}
	t.Fatalf("no file at %s among %v", path, files)
	return nodeFile{}
}

func TestContainerdTrustsTheEmulatorsCAForItsRegistryHost(t *testing.T) {
	files := testPulls().files()

	ca := fileAt(t, files, "/etc/containerd/certs.d/172.29.0.1:9597/ca.crt")
	if !bytes.Equal(ca.Content, testPulls().EmulatorCA) {
		t.Errorf("the registry's CA file holds %q", ca.Content)
	}
	hosts := string(fileAt(t, files, "/etc/containerd/certs.d/172.29.0.1:9597/hosts.toml").Content)
	for _, fragment := range []string{
		`server = "https://172.29.0.1:9597"`,
		`[host."https://172.29.0.1:9597"]`,
		`capabilities = ["pull", "resolve"]`,
		`ca = "/etc/containerd/certs.d/172.29.0.1:9597/ca.crt"`,
	} {
		if !strings.Contains(hosts, fragment) {
			t.Errorf("hosts.toml lacks %s:\n%s", fragment, hosts)
		}
	}
}

// credentialProviderConfigDocument mirrors the parts of kubelet.config.k8s.io/v1
// CredentialProviderConfig the kubelet acts on.
type credentialProviderConfigDocument struct {
	APIVersion string `json:"apiVersion"`
	Kind       string `json:"kind"`
	Providers  []struct {
		Name                 string   `json:"name"`
		MatchImages          []string `json:"matchImages"`
		DefaultCacheDuration string   `json:"defaultCacheDuration"`
		APIVersion           string   `json:"apiVersion"`
		Env                  []struct {
			Name  string `json:"name"`
			Value string `json:"value"`
		} `json:"env"`
	} `json:"providers"`
}

func TestTheKubeletAsksTheCredentialProviderForTheEmulatorsRegistryOnly(t *testing.T) {
	files := testPulls().files()
	var config credentialProviderConfigDocument
	if err := yaml.UnmarshalStrict(fileAt(t, files, "/etc/eks/image-credential-provider/config.yaml").Content, &config); err != nil {
		t.Fatal(err)
	}

	if config.APIVersion != "kubelet.config.k8s.io/v1" || config.Kind != "CredentialProviderConfig" || len(config.Providers) != 1 {
		t.Fatalf("not one kubelet.config.k8s.io/v1 provider: %+v", config)
	}
	provider := config.Providers[0]
	if provider.Name != "ecr-credential-provider" || provider.APIVersion != "credentialprovider.kubelet.k8s.io/v1" {
		t.Errorf("provider %q speaks %q", provider.Name, provider.APIVersion)
	}
	if strings.Join(provider.MatchImages, ",") != testRegistryHost {
		t.Errorf("the provider matches %v", provider.MatchImages)
	}
	if provider.DefaultCacheDuration == "" {
		t.Error("the provider names no default cache duration, which the kubelet requires")
	}
	env := map[string]string{}
	for _, variable := range provider.Env {
		env[variable.Name] = variable.Value
	}
	want := map[string]string{
		"REGISTRY_CREDENTIALS_URL":   "https://10.0.0.1:9598/registry-credentials/us-east-1/core",
		"REGISTRY_CREDENTIALS_CA":    "/etc/eks/image-credential-provider/provisioner-ca.crt",
		"KUBELET_CLIENT_CERTIFICATE": "/var/lib/kubelet/pki/kubelet-client-current.pem",
	}
	for name, value := range want {
		if env[name] != value {
			t.Errorf("%s is %q, want %q", name, env[name], value)
		}
	}
	plugin := fileAt(t, files, "/etc/eks/image-credential-provider/ecr-credential-provider")
	if plugin.Mode != "0755" || !strings.HasPrefix(string(plugin.Content), "#!/bin/sh\n") {
		t.Errorf("the plugin is not an executable script: mode %s", plugin.Mode)
	}
	if ca := fileAt(t, files, "/etc/eks/image-credential-provider/provisioner-ca.crt"); !bytes.Equal(ca.Content, testPulls().ProvisionerCA) {
		t.Errorf("the provisioner's CA file holds %q", ca.Content)
	}
}

func TestANodeJoinsWithTheImageCredentialProvider(t *testing.T) {
	config := joinConfiguration(testRegistration(), "10.0.1.20:6443", "abcdef.0123456789abcdef", "sha256:00")

	for _, fragment := range []string{
		`{name: image-credential-provider-config, value: "/etc/eks/image-credential-provider/config.yaml"}`,
		`{name: image-credential-provider-bin-dir, value: "/etc/eks/image-credential-provider"}`,
	} {
		if !strings.Contains(config, fragment) {
			t.Errorf("the JoinConfiguration lacks %s:\n%s", fragment, config)
		}
	}
}

func TestANodeJoinedWithoutTheProviderGetsItsFlagsOnce(t *testing.T) {
	joined := `KUBELET_KUBEADM_ARGS="--container-runtime-endpoint=unix:///run/containerd/containerd.sock --node-ip=10.0.1.7"` + "\n"

	enabled, changed, err := enableCredentialProvider(joined)
	if err != nil || !changed {
		t.Fatalf("changed %v, err %v", changed, err)
	}
	for _, flag := range []string{
		"--image-credential-provider-config=/etc/eks/image-credential-provider/config.yaml",
		"--image-credential-provider-bin-dir=/etc/eks/image-credential-provider",
		"--node-ip=10.0.1.7",
	} {
		if !strings.Contains(enabled, flag) {
			t.Errorf("the kubelet flags lack %s: %s", flag, enabled)
		}
	}
	again, changed, err := enableCredentialProvider(enabled)
	if err != nil || changed || again != enabled {
		t.Fatalf("a second pass changed the flags (%v, %v): %s", changed, err, again)
	}
}

func TestKubeletFlagsWithoutTheKubeadmVariableAreRefused(t *testing.T) {
	if _, _, err := enableCredentialProvider("KUBELET_EXTRA_ARGS=\"\"\n"); err == nil {
		t.Fatal("flags kubeadm did not write were rewritten")
	}
}

func TestTheProviderResponseCarriesTheTokensUserAndPassword(t *testing.T) {
	now := time.Date(2026, 11, 23, 12, 0, 0, 0, time.UTC)
	// base64("AWS:s3cr3t")
	response, err := credentialProviderResponse(testRegistryHost, "QVdTOnMzY3IzdA==", now.Add(12*time.Hour), now)
	if err != nil {
		t.Fatal(err)
	}

	if response.APIVersion != "credentialprovider.kubelet.k8s.io/v1" || response.Kind != "CredentialProviderResponse" || response.CacheKeyType != "Registry" {
		t.Errorf("response header %+v", response)
	}
	auth, ok := response.Auth[testRegistryHost]
	if !ok || auth.Username != "AWS" || auth.Password != "s3cr3t" || len(response.Auth) != 1 {
		t.Errorf("auth %+v", response.Auth)
	}
	if response.CacheDuration != registryCredentialCacheDuration.String() {
		t.Errorf("a 12 hour token is cached for %s", response.CacheDuration)
	}
}

func TestTheKubeletCachesATokenNoLongerThanItLives(t *testing.T) {
	now := time.Date(2026, 11, 23, 12, 0, 0, 0, time.UTC)
	response, err := credentialProviderResponse(testRegistryHost, "QVdTOnMzY3IzdA==", now.Add(4*time.Minute), now)
	if err != nil {
		t.Fatal(err)
	}

	cached, err := time.ParseDuration(response.CacheDuration)
	if err != nil {
		t.Fatal(err)
	}
	if cached <= 0 || cached >= 4*time.Minute {
		t.Fatalf("a token that expires in 4 minutes is cached for %s", cached)
	}
}

func TestATokenAboutToExpireIsNotHandedToTheKubelet(t *testing.T) {
	now := time.Date(2026, 11, 23, 12, 0, 0, 0, time.UTC)
	for _, expiresAt := range []time.Time{now.Add(-time.Minute), now.Add(30 * time.Second)} {
		if _, err := credentialProviderResponse(testRegistryHost, "QVdTOnMzY3IzdA==", expiresAt, now); err == nil {
			t.Errorf("a token expiring at %s was handed out at %s", expiresAt, now)
		}
	}
}

func TestATokenThatIsNotABasicCredentialIsRefused(t *testing.T) {
	now := time.Now()
	for _, token := range []string{"not base64!", "bm9jb2xvbg==" /* "nocolon" */} {
		if _, err := credentialProviderResponse(testRegistryHost, token, now.Add(time.Hour), now); err == nil {
			t.Errorf("token %q was handed out", token)
		}
	}
}

// testCA is a CA that signs client certificates, as a cluster's CA signs its kubelets'.
type testCA struct {
	cert *x509.Certificate
	key  *ecdsa.PrivateKey
	pem  []byte
}

func newTestCA(t *testing.T, name string) testCA {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	template := &x509.Certificate{
		SerialNumber: big.NewInt(1), Subject: pkix.Name{CommonName: name},
		NotBefore: time.Now().Add(-time.Hour), NotAfter: time.Now().Add(time.Hour),
		IsCA: true, BasicConstraintsValid: true, KeyUsage: x509.KeyUsageCertSign,
	}
	der, err := x509.CreateCertificate(rand.Reader, template, template, &key.PublicKey, key)
	if err != nil {
		t.Fatal(err)
	}
	cert, _ := x509.ParseCertificate(der)
	return testCA{cert: cert, key: key, pem: pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})}
}

// client issues a client certificate, and returns it with its key in one PEM, the way the
// kubelet keeps kubelet-client-current.pem.
func (ca testCA) client(t *testing.T, commonName string, organizations ...string) (*x509.Certificate, []byte) {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	template := &x509.Certificate{
		SerialNumber: big.NewInt(time.Now().UnixNano()), Subject: pkix.Name{CommonName: commonName, Organization: organizations},
		NotBefore: time.Now().Add(-time.Hour), NotAfter: time.Now().Add(time.Hour),
		KeyUsage: x509.KeyUsageDigitalSignature, ExtKeyUsage: []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth},
	}
	der, err := x509.CreateCertificate(rand.Reader, template, ca.cert, &key.PublicKey, ca.key)
	if err != nil {
		t.Fatal(err)
	}
	cert, _ := x509.ParseCertificate(der)
	keyDER, _ := x509.MarshalECPrivateKey(key)
	combined := append(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der}), pem.EncodeToMemory(&pem.Block{Type: "EC PRIVATE KEY", Bytes: keyDER})...)
	return cert, combined
}

func TestOnlyAKubeletOfTheClusterIsANode(t *testing.T) {
	cluster, other := newTestCA(t, "cluster"), newTestCA(t, "another cluster")
	kubelet, _ := cluster.client(t, "system:node:ip-10-0-1-7.ec2.internal", "system:nodes")
	admin, _ := cluster.client(t, "kubernetes-admin", "kubeadm:cluster-admins")
	foreign, _ := other.client(t, "system:node:ip-10-0-1-7.ec2.internal", "system:nodes")

	node, err := nodeIdentity([]*x509.Certificate{kubelet}, cluster.pem)
	if err != nil || node != "ip-10-0-1-7.ec2.internal" {
		t.Errorf("the cluster's kubelet is %q, %v", node, err)
	}
	cases := map[string][]*x509.Certificate{"no certificate": nil, "the cluster's administrator": {admin}, "another cluster's kubelet": {foreign}}
	for name, chain := range cases {
		if node, err := nodeIdentity(chain, cluster.pem); err == nil {
			t.Errorf("%s was taken for node %q", name, node)
		}
	}
}

// fakeECR answers GetAuthorizationToken with token, expiring at expiresAt.
func fakeECR(t *testing.T, token string, expiresAt time.Time) *httptest.Server {
	t.Helper()
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		if target := request.Header.Get("X-Amz-Target"); target != "AmazonEC2ContainerRegistry_V20150921.GetAuthorizationToken" {
			http.Error(writer, "unexpected "+target, http.StatusBadRequest)
			return
		}
		writer.Header().Set("Content-Type", "application/x-amz-json-1.1")
		json.NewEncoder(writer).Encode(map[string]any{"authorizationData": []map[string]any{{
			"authorizationToken": token,
			"expiresAt":          float64(expiresAt.Unix()),
			"proxyEndpoint":      "https://" + testRegistryHost,
		}}})
	}))
	t.Cleanup(server.Close)
	return server
}

func credentialsProvisioner(t *testing.T, ecrURL string, clusterCA []byte) *provisioner {
	t.Helper()
	p := &provisioner{
		stateDir:     t.TempDir(),
		registryHost: testRegistryHost,
		log:          slog.New(slog.NewTextHandler(io.Discard, nil)),
		awsConfig: aws.Config{
			Region:       "us-east-1",
			BaseEndpoint: aws.String(ecrURL),
			Credentials:  aws.CredentialsProviderFunc(func(context.Context) (aws.Credentials, error) { return aws.Credentials{AccessKeyID: "AKID", SecretAccessKey: "secret"}, nil }),
		},
	}
	spec := clusterSpec{StateDir: p.clusterDir("us-east-1", "core")}
	if err := os.MkdirAll(filepath.Dir(spec.caCert()), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(spec.caCert(), clusterCA, 0o644); err != nil {
		t.Fatal(err)
	}
	return p
}

const kubeletRequest = `{"apiVersion":"credentialprovider.kubelet.k8s.io/v1","kind":"CredentialProviderRequest","image":"172.29.0.1:9597/loopzero-tenants/acme/web-api@sha256:00"}`

func TestANodeOfTheClusterGetsAFreshRegistryToken(t *testing.T) {
	cluster := newTestCA(t, "cluster")
	kubelet, _ := cluster.client(t, "system:node:ip-10-0-1-7.ec2.internal", "system:nodes")
	ecr := fakeECR(t, "QVdTOnMzY3IzdA==", time.Now().Add(12*time.Hour))
	p := credentialsProvisioner(t, ecr.URL, cluster.pem)

	request := httptest.NewRequest(http.MethodPost, "/registry-credentials/us-east-1/core", strings.NewReader(kubeletRequest))
	request.TLS = &tls.ConnectionState{PeerCertificates: []*x509.Certificate{kubelet}}
	recorder := httptest.NewRecorder()
	p.serveRegistryCredentials(recorder, request)

	if recorder.Code != http.StatusOK {
		t.Fatalf("answered %d: %s", recorder.Code, recorder.Body)
	}
	var response providerResponse
	if err := json.Unmarshal(recorder.Body.Bytes(), &response); err != nil {
		t.Fatal(err)
	}
	if auth := response.Auth[testRegistryHost]; auth.Username != "AWS" || auth.Password != "s3cr3t" {
		t.Fatalf("auth %+v", response.Auth)
	}
}

func TestRegistryCredentialsAreRefusedToAnythingButANodeOfTheCluster(t *testing.T) {
	cluster, other := newTestCA(t, "cluster"), newTestCA(t, "another cluster")
	foreign, _ := other.client(t, "system:node:ip-10-0-1-7.ec2.internal", "system:nodes")
	ecr := fakeECR(t, "QVdTOnMzY3IzdA==", time.Now().Add(12*time.Hour))
	p := credentialsProvisioner(t, ecr.URL, cluster.pem)

	for name, state := range map[string]*tls.ConnectionState{
		"a pod with no certificate": {},
		"another cluster's kubelet": {PeerCertificates: []*x509.Certificate{foreign}},
	} {
		request := httptest.NewRequest(http.MethodPost, "/registry-credentials/us-east-1/core", strings.NewReader(kubeletRequest))
		request.TLS = state
		recorder := httptest.NewRecorder()
		p.serveRegistryCredentials(recorder, request)
		if recorder.Code != http.StatusForbidden || strings.Contains(recorder.Body.String(), "s3cr3t") {
			t.Errorf("%s: answered %d: %s", name, recorder.Code, recorder.Body)
		}
	}
}

func TestAnImageOfAnotherRegistryGetsNoCredentials(t *testing.T) {
	cluster := newTestCA(t, "cluster")
	kubelet, _ := cluster.client(t, "system:node:ip-10-0-1-7.ec2.internal", "system:nodes")
	ecr := fakeECR(t, "QVdTOnMzY3IzdA==", time.Now().Add(12*time.Hour))
	p := credentialsProvisioner(t, ecr.URL, cluster.pem)

	body := `{"apiVersion":"credentialprovider.kubelet.k8s.io/v1","kind":"CredentialProviderRequest","image":"ghcr.io/acme/web-api:1"}`
	request := httptest.NewRequest(http.MethodPost, "/registry-credentials/us-east-1/core", strings.NewReader(body))
	request.TLS = &tls.ConnectionState{PeerCertificates: []*x509.Certificate{kubelet}}
	recorder := httptest.NewRecorder()
	p.serveRegistryCredentials(recorder, request)

	if recorder.Code != http.StatusBadRequest || strings.Contains(recorder.Body.String(), "s3cr3t") {
		t.Fatalf("answered %d: %s", recorder.Code, recorder.Body)
	}
}

// TestThePluginHandsTheKubeletsRequestToTheProvisioner runs the plugin script the node gets
// with curl, as the kubelet runs it: the request on stdin, the provider's env, and the
// answer on stdout.
func TestThePluginHandsTheKubeletsRequestToTheProvisioner(t *testing.T) {
	if _, err := exec.LookPath("curl"); err != nil {
		t.Skip("curl is not installed")
	}
	cluster := newTestCA(t, "cluster")
	_, kubeletPEM := cluster.client(t, "system:node:ip-10-0-1-7.ec2.internal", "system:nodes")
	var received string
	server := httptest.NewUnstartedServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		body, _ := io.ReadAll(request.Body)
		received = string(body)
		if request.TLS == nil || len(request.TLS.PeerCertificates) == 0 || request.Method != http.MethodPost {
			http.Error(writer, "no client certificate", http.StatusForbidden)
			return
		}
		writer.Write([]byte(`{"kind":"CredentialProviderResponse"}`))
	}))
	server.TLS = &tls.Config{ClientAuth: tls.RequestClientCert}
	server.StartTLS()
	defer server.Close()
	dir := t.TempDir()
	write := func(name string, content []byte, mode os.FileMode) string {
		path := filepath.Join(dir, name)
		if err := os.WriteFile(path, content, mode); err != nil {
			t.Fatal(err)
		}
		return path
	}
	serverCA := pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: server.Certificate().Raw})
	plugin := write("ecr-credential-provider", []byte(credentialProviderPlugin), 0o755)

	command := exec.Command(plugin)
	command.Stdin = strings.NewReader(kubeletRequest)
	command.Env = append(os.Environ(),
		"REGISTRY_CREDENTIALS_URL="+server.URL+"/registry-credentials/us-east-1/core",
		"REGISTRY_CREDENTIALS_CA="+write("ca.crt", serverCA, 0o644),
		"KUBELET_CLIENT_CERTIFICATE="+write("kubelet.pem", kubeletPEM, 0o600))
	var stderr bytes.Buffer
	command.Stderr = &stderr
	output, err := command.Output()
	if err != nil {
		t.Fatalf("the plugin failed: %v: %s", err, stderr.String())
	}
	if string(output) != `{"kind":"CredentialProviderResponse"}` || received != kubeletRequest {
		t.Fatalf("the plugin printed %q after sending %q", output, received)
	}
}
