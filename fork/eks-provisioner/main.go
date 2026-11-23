// SPDX-License-Identifier: CC0-1.0

// Command eks-provisioner runs the Kubernetes control plane and the nodes of every EKS
// cluster the emulator holds, as Amazon EKS runs them behind its API: an emulator started
// with EMULATOR_EKS_CONTROL_PLANE=external leaves a cluster CREATING, and this provisioner
// brings up a kind control plane that signs with the cluster's CA and issues tokens as its
// OIDC issuer, registers it, and runs each running instance of the cluster's managed node
// groups as a node at the instance's address. It also answers the API servers' token
// reviews, so a kubeconfig from `aws eks update-kubeconfig` works with the cluster's
// access entries.
//
// It reads the emulator from the standard AWS environment: AWS_ENDPOINT_URL,
// AWS_CA_BUNDLE, and the emulator's own keys (AWS_SHARED_CREDENTIALS_FILE).
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"log/slog"
	"net/netip"
	"os"
	"os/signal"
	"path/filepath"
	"regexp"
	"strings"
	"syscall"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/service/ec2"
	"github.com/aws/aws-sdk-go-v2/service/eks"
	ekstypes "github.com/aws/aws-sdk-go-v2/service/eks/types"
	"sigs.k8s.io/kind/pkg/cluster"
	"sigs.k8s.io/kind/pkg/log"
)

type provisioner struct {
	regions    []string
	stateDir   string
	listenPort string
	emulator   *emulator
	webhook    *webhook
	kind       *cluster.Provider
	awsConfig  aws.Config
	log        *slog.Logger
	retryAfter map[string]time.Time
	failures   map[string]int
	// trust is the CA every pod of every cluster gets, and podEndpoint the emulator's
	// address they get, which they reach through their VPC's gateway.
	trust       trustBundle
	podEndpoint string
	addonsAt    map[string]time.Time
	// webhookImage is the pod identity webhook image every cluster runs.
	webhookImage string
	// registryHost is the host:port of the emulator's ECR registry that nodes pull from:
	// the host of AWS_ENDPOINT_URL, which they reach as pods do.
	registryHost string
	// pullsConfigured are the node containers this run configured to pull from it.
	pullsConfigured map[string]bool
}

func main() {
	regions := flag.String("regions", envOr("AWS_REGION", "us-east-1"), "comma-separated Regions whose clusters to provision")
	stateDir := flag.String("state-dir", "/var/lib/aws-emulator-eks", "where each cluster's CA, kubeconfig and webhook configuration are kept")
	listen := flag.String("listen", ":9598", "address the token webhook listens on; the API servers dial the gateway of their VPC's bridge at this port")
	interval := flag.Duration("interval", 5*time.Second, "time between two reconcile passes")
	stsAliases := flag.String("sts-aliases", "", "comma-separated host:port names, besides AWS_ENDPOINT_URL's, that clients reach the same emulator by (localhost:9597 on the emulator's host); a token signed for one of them is accepted")
	systemRoots := flag.String("system-roots", "", "PEM bundle of public roots that pods get with the emulator's CA (default: SSL_CERT_FILE, else the host's bundle)")
	webhookImage := flag.String("pod-identity-webhook-image", envOr("POD_IDENTITY_WEBHOOK_IMAGE", defaultPodIdentityWebhookImage), "the amazon-eks-pod-identity-webhook image, pinned by digest; a copy of the default for a host ECR Public throttles")
	flag.Parse()

	logger := slog.New(slog.NewJSONHandler(os.Stderr, nil))
	if err := run(*regions, *stateDir, *listen, *stsAliases, *systemRoots, *webhookImage, *interval, logger); err != nil {
		logger.Error("eks-provisioner stopped", "error", err)
		os.Exit(1)
	}
}

func run(regions, stateDir, listen, stsAliases, systemRoots, webhookImage string, interval time.Duration, logger *slog.Logger) error {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	endpoint := os.Getenv("AWS_ENDPOINT_URL")
	if endpoint == "" {
		return errors.New("AWS_ENDPOINT_URL is not set; it names the emulator")
	}
	awsConfig, err := config.LoadDefaultConfig(ctx)
	if err != nil {
		return fmt.Errorf("loading the AWS configuration: %w", err)
	}
	emu, err := newEmulator(endpoint, splitList(stsAliases), os.Getenv("AWS_CA_BUNDLE"), awsConfig.Credentials)
	if err != nil {
		return err
	}
	hook, err := loadWebhookCA(stateDir)
	if err != nil {
		return err
	}
	if systemRoots == "" {
		if systemRoots, err = findSystemRoots(); err != nil {
			return err
		}
	}
	trust, err := loadTrustBundle(os.Getenv("AWS_CA_BUNDLE"), systemRoots)
	if err != nil {
		return err
	}
	if err := checkWebhookImage(webhookImage); err != nil {
		return err
	}
	_, port, found := strings.Cut(listen, ":")
	if !found || port == "" {
		return fmt.Errorf("--listen %q names no port", listen)
	}
	p := &provisioner{
		regions:         splitList(regions),
		stateDir:        stateDir,
		listenPort:      port,
		emulator:        emu,
		webhook:         hook,
		kind:            cluster.NewProvider(cluster.ProviderWithDocker(), cluster.ProviderWithLogger(log.NoopLogger{})),
		awsConfig:       awsConfig,
		log:             logger,
		retryAfter:      map[string]time.Time{},
		failures:        map[string]int{},
		trust:           trust,
		podEndpoint:     strings.TrimRight(endpoint, "/"),
		addonsAt:        map[string]time.Time{},
		webhookImage:    webhookImage,
		registryHost:    emu.endpoint.Host,
		pullsConfigured: map[string]bool{},
	}
	served := make(chan error, 1)
	go func() { served <- p.listen(ctx, listen, logger) }()

	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		for _, region := range p.regions {
			if err := p.reconcileRegion(ctx, region); err != nil {
				logger.Error("reconcile pass", "region", region, "error", err)
			}
		}
		select {
		case <-ctx.Done():
			return nil
		case err := <-served:
			return webhookStopped(ctx, err)
		case <-ticker.C:
		}
	}
}

// reconcileRegion makes the kind clusters of a Region match its EKS clusters.
func (p *provisioner) reconcileRegion(ctx context.Context, region string) error {
	client := p.eks(region)
	paginator := eks.NewListClustersPaginator(client, &eks.ListClustersInput{})
	present := map[string]bool{}
	for paginator.HasMorePages() {
		page, err := paginator.NextPage(ctx)
		if err != nil {
			return fmt.Errorf("listing clusters: %w", err)
		}
		for _, name := range page.Clusters {
			described, err := client.DescribeCluster(ctx, &eks.DescribeClusterInput{Name: aws.String(name)})
			if err != nil {
				p.log.Error("describing a cluster", "cluster", name, "region", region, "error", err)
				present[name] = true
				continue
			}
			cluster := described.Cluster
			if cluster.ConnectorConfig != nil {
				continue
			}
			if cluster.Status == ekstypes.ClusterStatusDeleting {
				continue
			}
			present[name] = true
			p.attempt(ctx, region, name, func() error { return p.reconcileCluster(ctx, region, cluster) })
		}
	}
	owned, err := p.kind.List()
	if err != nil {
		return fmt.Errorf("listing kind clusters: %w", err)
	}
	prefix := kindName(region, "")
	for _, kind := range owned {
		name, ours := strings.CutPrefix(kind, prefix)
		if !ours || present[name] || !p.provisioned(region, name) {
			continue
		}
		p.attempt(ctx, region, name, func() error { return p.teardown(ctx, region, name) })
	}
	return nil
}

// reconcileCluster provisions a cluster's control plane when it has none, and its
// add-ons and nodes once it is ACTIVE.
func (p *provisioner) reconcileCluster(ctx context.Context, region string, described *ekstypes.Cluster) error {
	name := aws.ToString(described.Name)
	if described.Status == ekstypes.ClusterStatusCreating || !p.kindExists(kindName(region, name)) || !p.registered(described) {
		return p.provisionControlPlane(ctx, region, described)
	}
	if err := p.reconcileAddons(ctx, region, described); err != nil {
		return err
	}
	return p.reconcileNodes(ctx, region, name)
}

// provisioned is whether this provisioner built the kind cluster of an EKS cluster: it
// keeps the cluster's state directory. A kind cluster another provisioner built, beside
// another emulator on the same host, is not this one's to tear down.
func (p *provisioner) provisioned(region, name string) bool {
	_, err := os.Stat(p.clusterDir(region, name))
	return err == nil
}

// registered is whether the cluster's endpoint is the API server this provisioner ran.
func (p *provisioner) registered(described *ekstypes.Cluster) bool {
	endpoint := aws.ToString(described.Endpoint)
	host, ok := strings.CutPrefix(endpoint, "https://")
	if !ok {
		return false
	}
	address, err := netip.ParseAddrPort(host)
	return err == nil && address.Port() == apiServerPort
}

func (p *provisioner) teardown(ctx context.Context, region, name string) error {
	p.log.Info("deleting the control plane of a cluster that is gone", "cluster", name, "region", region)
	delete(p.addonsAt, region+"/"+name)
	for container := range p.pullsConfigured {
		if strings.HasPrefix(container, kindName(region, name)+"-") {
			delete(p.pullsConfigured, container)
		}
	}
	if err := p.kind.Delete(kindName(region, name), filepath.Join(p.clusterDir(region, name), "kubeconfig")); err != nil {
		return fmt.Errorf("kind delete cluster %s: %w", kindName(region, name), err)
	}
	if err := p.deleteControlPlaneInterface(ctx, region, name); err != nil {
		p.log.Warn("the control-plane interface was not deleted", "cluster", name, "error", err)
	}
	return os.RemoveAll(p.clusterDir(region, name))
}

// attempt runs one cluster's step, and backs off a cluster that keeps failing so one
// broken cluster neither floods the log nor holds up the others.
func (p *provisioner) attempt(ctx context.Context, region, name string, step func() error) {
	key := region + "/" + name
	if time.Now().Before(p.retryAfter[key]) {
		return
	}
	if err := step(); err != nil {
		p.failures[key]++
		delay := min(time.Duration(1<<min(p.failures[key], 8))*time.Second, 5*time.Minute)
		p.retryAfter[key] = time.Now().Add(delay)
		p.log.Error("cluster step failed", "cluster", name, "region", region, "attempt", p.failures[key], "retry_in", delay.String(), "error", err)
		return
	}
	delete(p.failures, key)
	delete(p.retryAfter, key)
}

func (p *provisioner) kindExists(name string) bool {
	clusters, err := p.kind.List()
	if err != nil {
		return false
	}
	for _, existing := range clusters {
		if existing == name {
			return true
		}
	}
	return false
}

func (p *provisioner) clusterDir(region, name string) string {
	return filepath.Join(p.stateDir, "clusters", region, name)
}

func (p *provisioner) webhookURL(gateway netip.Addr, region, name string) string {
	return fmt.Sprintf("https://%s:%s/authenticate/%s/%s", gateway, p.listenPort, region, name)
}

func (p *provisioner) eks(region string) *eks.Client {
	return eks.NewFromConfig(p.awsConfig, func(options *eks.Options) { options.Region = region })
}

func (p *provisioner) ec2(region string) *ec2.Client {
	return ec2.NewFromConfig(p.awsConfig, func(options *ec2.Options) { options.Region = region })
}

var kindNamePattern = regexp.MustCompile(`^[a-z0-9]([a-z0-9.-]*[a-z0-9])?$`)

// kindName is the kind cluster of an EKS cluster, unique across Regions.
func kindName(region, cluster string) string {
	return "eks-" + region + "-" + cluster
}

func splitList(text string) []string {
	var out []string
	for _, item := range strings.Split(text, ",") {
		if item = strings.TrimSpace(item); item != "" {
			out = append(out, item)
		}
	}
	return out
}

func envOr(name, fallback string) string {
	if value := os.Getenv(name); value != "" {
		return value
	}
	return fallback
}

// webhookStopped is what the provisioner exits with once its webhook server returned: nothing
// on shutdown, which returns the server without an error, and an error otherwise.
func webhookStopped(ctx context.Context, err error) error {
	switch {
	case err != nil:
		return fmt.Errorf("the token webhook stopped: %w", err)
	case ctx.Err() != nil:
		return nil
	default:
		return errors.New("the token webhook stopped without an error")
	}
}
