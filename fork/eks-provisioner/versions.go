// SPDX-License-Identifier: CC0-1.0

package main

import (
	"fmt"
	"sort"
	"strings"
)

// nodeImages are the kind node images this provisioner runs, by Kubernetes minor version:
// the images kind v0.31.0 publishes, pinned by digest (its release notes list them).
var nodeImages = map[string]string{
	"1.35": "kindest/node:v1.35.0@sha256:452d707d4862f52530247495d180205e029056831160e22870e37e3f6c1ac31f",
	"1.34": "kindest/node:v1.34.3@sha256:08497ee19eace7b4b5348db5c6a1591d7752b164530a36f855cb0f2bdcbadd48",
	"1.33": "kindest/node:v1.33.7@sha256:d26ef333bdb2cbe9862a0f7c3803ecc7b4303d8cea8e814b481b09949d353040",
	"1.32": "kindest/node:v1.32.11@sha256:5fc52d52a7b9574015299724bd68f183702956aa4a2116ae75a63cb574b35af8",
	"1.31": "kindest/node:v1.31.14@sha256:6f86cf509dbb42767b6e79debc3f2c32e4ee01386f0489b3b2be24b0a55aac2b",
}

// nodeImage is the kind node image for an EKS cluster's Kubernetes version ("1.34"), or an
// error naming the versions that have one.
func nodeImage(version string) (string, error) {
	if image, ok := nodeImages[strings.TrimSpace(version)]; ok {
		return image, nil
	}
	supported := make([]string, 0, len(nodeImages))
	for minor := range nodeImages {
		supported = append(supported, minor)
	}
	sort.Strings(supported)
	return "", fmt.Errorf("Kubernetes %q has no kind node image here; create the cluster with one of %s", version, strings.Join(supported, ", "))
}
