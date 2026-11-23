// SPDX-License-Identifier: CC0-1.0

package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/pem"
	"fmt"
	"os"

	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/kubernetes"
)

const (
	// emulatorCAKey holds the emulator's CA alone.
	emulatorCAKey = "ca.pem"
	// combinedBundleKey holds the public roots and the emulator's CA.
	combinedBundleKey = "ca-bundle.pem"
)

// systemRootFiles are where Linux distributions keep their root bundle, the files Go's
// crypto/x509 reads on Linux.
var systemRootFiles = []string{
	"/etc/ssl/certs/ca-certificates.crt",
	"/etc/pki/tls/certs/ca-bundle.crt",
	"/etc/ssl/ca-bundle.pem",
	"/etc/pki/tls/cacert.pem",
	"/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
	"/etc/ssl/cert.pem",
}

// trustBundle is what a pod trusts: the emulator's CA, and the public roots with it.
type trustBundle struct {
	EmulatorCA []byte
	Combined   []byte
}

// findSystemRoots is the host's root bundle: SSL_CERT_FILE when set, else the first of
// systemRootFiles that exists.
func findSystemRoots() (string, error) {
	if path := os.Getenv("SSL_CERT_FILE"); path != "" {
		return path, nil
	}
	for _, path := range systemRootFiles {
		if _, err := os.Stat(path); err == nil {
			return path, nil
		}
	}
	return "", fmt.Errorf("no root certificate bundle in any of %v; name one with --system-roots", systemRootFiles)
}

func loadTrustBundle(emulatorCAPath, systemRootsPath string) (trustBundle, error) {
	emulatorCA, err := readCertificates(emulatorCAPath, "the emulator's CA (AWS_CA_BUNDLE)")
	if err != nil {
		return trustBundle{}, err
	}
	roots, err := readCertificates(systemRootsPath, "the system root bundle")
	if err != nil {
		return trustBundle{}, err
	}
	combined := append(bytes.TrimRight(roots, "\n"), '\n')
	return trustBundle{EmulatorCA: emulatorCA, Combined: append(combined, emulatorCA...)}, nil
}

func readCertificates(path, what string) ([]byte, error) {
	if path == "" {
		return nil, fmt.Errorf("%s is not set", what)
	}
	content, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("reading %s: %w", what, err)
	}
	for rest := content; ; {
		var block *pem.Block
		block, rest = pem.Decode(rest)
		if block == nil {
			return nil, fmt.Errorf("%s %s holds no PEM certificate", what, path)
		}
		if block.Type == "CERTIFICATE" {
			return content, nil
		}
	}
}

// configMapName names the ConfigMap after its content, so a CA that changes is a new,
// immutable ConfigMap that new pods mount, and a running pod keeps the one it started with.
func (bundle trustBundle) configMapName() string {
	digest := sha256.New()
	digest.Write(bundle.EmulatorCA)
	digest.Write([]byte{0})
	digest.Write(bundle.Combined)
	return "aws-emulator-ca-" + hex.EncodeToString(digest.Sum(nil))[:10]
}

// ensureTrustBundle creates the bundle's ConfigMap in namespace unless it is there. Two
// pods admitted at once both create it, and the second create's AlreadyExists is the
// answer that it is there.
func ensureTrustBundle(ctx context.Context, clientset kubernetes.Interface, bundle trustBundle, namespace string) error {
	immutable := true
	configMap := &corev1.ConfigMap{
		ObjectMeta: metav1.ObjectMeta{
			Name:      bundle.configMapName(),
			Namespace: namespace,
			Labels:    map[string]string{managedByLabel: managedBy},
		},
		Immutable: &immutable,
		Data:      map[string]string{emulatorCAKey: string(bundle.EmulatorCA), combinedBundleKey: string(bundle.Combined)},
	}
	_, err := clientset.CoreV1().ConfigMaps(namespace).Create(ctx, configMap, metav1.CreateOptions{})
	if err == nil || apierrors.IsAlreadyExists(err) {
		return nil
	}
	return fmt.Errorf("creating ConfigMap %s/%s: %w", namespace, configMap.Name, err)
}
