// SPDX-License-Identifier: CC0-1.0

package main

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/json"
	"encoding/pem"
	"errors"
	"fmt"
	"log/slog"
	"math/big"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/eks"
	ekstypes "github.com/aws/aws-sdk-go-v2/service/eks/types"
	authenticationv1 "k8s.io/api/authentication/v1"
)

// webhook answers the API servers' TokenReviews: who an `aws eks get-token` token belongs
// to, from STS, and what the cluster's access entries make of that principal.
type webhook struct {
	caPEM []byte
	caKey *ecdsa.PrivateKey
	ca    *x509.Certificate

	mu    sync.Mutex
	certs map[string]*tls.Certificate
}

// loadWebhookCA reads the webhook's CA from dir, or issues and stores one. It outlives a
// restart because every API server's webhook kubeconfig trusts it.
func loadWebhookCA(dir string) (*webhook, error) {
	certPath, keyPath := filepath.Join(dir, "webhook-ca.crt"), filepath.Join(dir, "webhook-ca.key")
	certPEM, certErr := os.ReadFile(certPath)
	keyPEM, keyErr := os.ReadFile(keyPath)
	if errors.Is(certErr, os.ErrNotExist) && errors.Is(keyErr, os.ErrNotExist) {
		key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
		if err != nil {
			return nil, err
		}
		template := &x509.Certificate{
			SerialNumber:          big.NewInt(time.Now().UnixNano()),
			Subject:               pkix.Name{CommonName: "aws-emulator EKS token webhook"},
			NotBefore:             time.Now().Add(-time.Hour),
			NotAfter:              time.Now().AddDate(10, 0, 0),
			IsCA:                  true,
			BasicConstraintsValid: true,
			KeyUsage:              x509.KeyUsageCertSign | x509.KeyUsageDigitalSignature,
		}
		der, err := x509.CreateCertificate(rand.Reader, template, template, &key.PublicKey, key)
		if err != nil {
			return nil, err
		}
		keyDER, err := x509.MarshalECPrivateKey(key)
		if err != nil {
			return nil, err
		}
		certPEM = pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})
		keyPEM = pem.EncodeToMemory(&pem.Block{Type: "EC PRIVATE KEY", Bytes: keyDER})
		if err := os.MkdirAll(dir, 0o700); err != nil {
			return nil, err
		}
		if err := os.WriteFile(keyPath, keyPEM, 0o600); err != nil {
			return nil, err
		}
		if err := os.WriteFile(certPath, certPEM, 0o644); err != nil {
			return nil, err
		}
	} else if certErr != nil || keyErr != nil {
		return nil, fmt.Errorf("the webhook CA in %s is incomplete: %v, %v", dir, certErr, keyErr)
	}
	certBlock, _ := pem.Decode(certPEM)
	keyBlock, _ := pem.Decode(keyPEM)
	if certBlock == nil || keyBlock == nil {
		return nil, fmt.Errorf("the webhook CA in %s is not PEM", dir)
	}
	ca, err := x509.ParseCertificate(certBlock.Bytes)
	if err != nil {
		return nil, err
	}
	key, err := x509.ParseECPrivateKey(keyBlock.Bytes)
	if err != nil {
		return nil, err
	}
	return &webhook{caPEM: certPEM, caKey: key, ca: ca, certs: map[string]*tls.Certificate{}}, nil
}

// certificateFor serves, on each local address an API server dials, a certificate for
// that address: an API server dials the gateway of its VPC's bridge, and sends no SNI.
func (w *webhook) certificateFor(hello *tls.ClientHelloInfo) (*tls.Certificate, error) {
	host, _, err := net.SplitHostPort(hello.Conn.LocalAddr().String())
	if err != nil {
		return nil, err
	}
	w.mu.Lock()
	defer w.mu.Unlock()
	if cert, ok := w.certs[host]; ok {
		return cert, nil
	}
	der, key, err := w.sign(host, nil, []net.IP{net.ParseIP(host)})
	if err != nil {
		return nil, err
	}
	cert := &tls.Certificate{Certificate: [][]byte{der}, PrivateKey: key}
	w.certs[host] = cert
	return cert, nil
}

// issue is a serving certificate for dnsNames and ips signed by the webhook CA, in PEM,
// for a server the API servers call other than this one.
func (w *webhook) issue(commonName string, dnsNames []string, ips []net.IP) (certPEM, keyPEM []byte, err error) {
	der, key, err := w.sign(commonName, dnsNames, ips)
	if err != nil {
		return nil, nil, err
	}
	keyDER, err := x509.MarshalECPrivateKey(key)
	if err != nil {
		return nil, nil, err
	}
	return pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der}), pem.EncodeToMemory(&pem.Block{Type: "EC PRIVATE KEY", Bytes: keyDER}), nil
}

// sign issues a one-year serving certificate signed by the webhook CA.
func (w *webhook) sign(commonName string, dnsNames []string, ips []net.IP) ([]byte, *ecdsa.PrivateKey, error) {
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return nil, nil, err
	}
	template := &x509.Certificate{
		SerialNumber: big.NewInt(time.Now().UnixNano()),
		Subject:      pkix.Name{CommonName: commonName},
		NotBefore:    time.Now().Add(-time.Hour),
		NotAfter:     time.Now().AddDate(1, 0, 0),
		KeyUsage:     x509.KeyUsageDigitalSignature,
		ExtKeyUsage:  []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
		DNSNames:     dnsNames,
		IPAddresses:  ips,
	}
	der, err := x509.CreateCertificate(rand.Reader, template, w.ca, &key.PublicKey, w.caKey)
	if err != nil {
		return nil, nil, err
	}
	return der, key, nil
}

// serveTokenReviews answers POST /authenticate/{region}/{cluster}.
func (p *provisioner) serveTokenReviews(writer http.ResponseWriter, request *http.Request) {
	region, cluster, ok := strings.Cut(strings.TrimPrefix(request.URL.Path, "/authenticate/"), "/")
	if request.Method != http.MethodPost || !ok || region == "" || cluster == "" {
		http.Error(writer, "POST /authenticate/{region}/{cluster}", http.StatusNotFound)
		return
	}
	var review authenticationv1.TokenReview
	if err := json.NewDecoder(http.MaxBytesReader(writer, request.Body, 64<<10)).Decode(&review); err != nil {
		http.Error(writer, "the body is not a TokenReview: "+err.Error(), http.StatusBadRequest)
		return
	}
	user, err := p.authenticate(request.Context(), region, cluster, review.Spec.Token)
	review.Status = authenticationv1.TokenReviewStatus{}
	if err != nil {
		p.log.Info("token refused", "cluster", cluster, "region", region, "reason", err.Error())
		review.Status.Error = err.Error()
	} else {
		review.Status.Authenticated = true
		review.Status.User = user
	}
	review.Spec = authenticationv1.TokenReviewSpec{}
	writer.Header().Set("Content-Type", "application/json")
	if err := json.NewEncoder(writer).Encode(review); err != nil {
		p.log.Error("answering a TokenReview", "error", err)
	}
}

// authenticate is the user a token stands for in a cluster: the principal STS reports for
// its presigned request, as the cluster's access entry for that principal names it.
func (p *provisioner) authenticate(ctx context.Context, region, cluster, token string) (authenticationv1.UserInfo, error) {
	presigned, err := parseToken(token, p.emulator.stsHosts, time.Now())
	if err != nil {
		return authenticationv1.UserInfo{}, err
	}
	caller, err := p.emulator.callerOf(ctx, presigned, cluster)
	if err != nil {
		return authenticationv1.UserInfo{}, err
	}
	principal, err := principalARN(caller.Arn)
	if err != nil {
		return authenticationv1.UserInfo{}, err
	}
	client := p.eks(region)
	entry, err := client.DescribeAccessEntry(ctx, &eks.DescribeAccessEntryInput{ClusterName: aws.String(cluster), PrincipalArn: aws.String(principal)})
	if err != nil {
		var missing *ekstypes.ResourceNotFoundException
		if errors.As(err, &missing) {
			return authenticationv1.UserInfo{}, fmt.Errorf("cluster %s has no access entry for %s", cluster, principal)
		}
		return authenticationv1.UserInfo{}, fmt.Errorf("reading the access entry of %s: %w", principal, err)
	}
	groups := append([]string{}, entry.AccessEntry.KubernetesGroups...)
	policies, err := client.ListAssociatedAccessPolicies(ctx, &eks.ListAssociatedAccessPoliciesInput{ClusterName: aws.String(cluster), PrincipalArn: aws.String(principal)})
	if err != nil {
		return authenticationv1.UserInfo{}, fmt.Errorf("reading the access policies of %s: %w", principal, err)
	}
	for _, association := range policies.AssociatedAccessPolicies {
		scope := association.AccessScope
		policyARN := aws.ToString(association.PolicyArn)
		if scope == nil || scope.Type == ekstypes.AccessScopeTypeCluster {
			group, _, err := policyGroup(policyARN, "")
			if err != nil {
				p.log.Warn("access policy not granted", "cluster", cluster, "principal", principal, "reason", err.Error())
				continue
			}
			groups = append(groups, group)
			continue
		}
		for _, namespace := range scope.Namespaces {
			group, policy, err := policyGroup(policyARN, namespace)
			if err != nil {
				p.log.Warn("access policy not granted", "cluster", cluster, "principal", principal, "reason", err.Error())
				break
			}
			if err := p.bindNamespaced(ctx, region, cluster, policy, namespace); err != nil {
				return authenticationv1.UserInfo{}, err
			}
			groups = append(groups, group)
		}
	}
	username := aws.ToString(entry.AccessEntry.Username)
	if username == "" {
		username = caller.Arn
	}
	return authenticationv1.UserInfo{
		Username: expandUsername(username, caller.Arn, caller.Account),
		UID:      "aws-iam-authenticator:" + caller.Account + ":" + caller.UserID,
		Groups:   groups,
		Extra: map[string]authenticationv1.ExtraValue{
			"arn":                  {caller.Arn},
			"canonicalArn":         {principal},
			"sessionName":          {sessionName(caller.Arn)},
			"principalId":          {caller.UserID},
			"accountId":            {caller.Account},
			"eks.amazonaws.com/id": {cluster},
		},
	}, nil
}

func (p *provisioner) bindNamespaced(ctx context.Context, region, cluster, policy, namespace string) error {
	clientset, err := p.clientset(region, cluster)
	if err != nil {
		return err
	}
	return bindNamespacedPolicy(ctx, clientset, policy, namespace)
}

// listen serves the token webhook, the endpoint injection webhook and /healthz until
// ctx ends.
func (p *provisioner) listen(ctx context.Context, address string, log *slog.Logger) error {
	mux := http.NewServeMux()
	mux.HandleFunc("/authenticate/", p.serveTokenReviews)
	mux.HandleFunc("/mutate/", p.servePodMutations)
	mux.HandleFunc("/healthz", func(writer http.ResponseWriter, _ *http.Request) { writer.Write([]byte("ok\n")) })
	server := &http.Server{
		Addr:              address,
		Handler:           mux,
		ReadHeaderTimeout: 10 * time.Second,
		TLSConfig:         &tls.Config{GetCertificate: p.webhook.certificateFor, MinVersion: tls.VersionTLS12},
	}
	go func() {
		<-ctx.Done()
		shutdown, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		server.Shutdown(shutdown)
	}()
	log.Info("token webhook listening", "address", address)
	if err := server.ListenAndServeTLS("", ""); err != nil && !errors.Is(err, http.ErrServerClosed) {
		return err
	}
	return nil
}
