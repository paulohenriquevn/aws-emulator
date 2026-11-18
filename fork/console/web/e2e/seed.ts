/**
 * Creates one resource of every type the console lists, through the AWS SDK, and
 * reports what each list page must then show.
 *
 * The end-to-end setup seeds the emulator it starts. To fill a running emulator by hand:
 *
 *   node --experimental-strip-types e2e/seed.ts https://localhost:9297 ~/.aws/emulator/9297
 *
 * Run it once against a fresh emulator: the names are fixed, so a second run fails on
 * resources that already exist.
 */

import { ACMClient, RequestCertificateCommand } from "@aws-sdk/client-acm";
import { AutoScalingClient, CreateAutoScalingGroupCommand } from "@aws-sdk/client-auto-scaling";
import { BudgetsClient, CreateBudgetCommand } from "@aws-sdk/client-budgets";
import { CloudFrontClient, CreateDistributionCommand } from "@aws-sdk/client-cloudfront";
import { CloudWatchClient, PutMetricAlarmCommand } from "@aws-sdk/client-cloudwatch";
import { CloudWatchLogsClient, CreateLogGroupCommand, CreateLogStreamCommand } from "@aws-sdk/client-cloudwatch-logs";
import {
  AllocateAddressCommand,
  AttachInternetGatewayCommand,
  CreateInternetGatewayCommand,
  CreateLaunchTemplateCommand,
  CreateNatGatewayCommand,
  CreateNetworkAclCommand,
  CreateRouteTableCommand,
  CreateSecurityGroupCommand,
  CreateSubnetCommand,
  CreateVpcCommand,
  EC2Client,
  RunInstancesCommand,
  type TagSpecification,
} from "@aws-sdk/client-ec2";
import {
  CompleteLayerUploadCommand,
  CreateRepositoryCommand,
  ECRClient,
  InitiateLayerUploadCommand,
  PutImageCommand,
  UploadLayerPartCommand,
} from "@aws-sdk/client-ecr";
import { CreateClusterCommand, CreateNodegroupCommand, DescribeClusterCommand, EKSClient } from "@aws-sdk/client-eks";
import { CreateCacheClusterCommand, CreateCacheSubnetGroupCommand, CreateReplicationGroupCommand, ElastiCacheClient } from "@aws-sdk/client-elasticache";
import {
  CreateListenerCommand,
  CreateLoadBalancerCommand,
  CreateTargetGroupCommand,
  ElasticLoadBalancingV2Client,
} from "@aws-sdk/client-elastic-load-balancing-v2";
import { CreatePolicyCommand, CreateRoleCommand, CreateUserCommand, IAMClient } from "@aws-sdk/client-iam";
import { CreateAliasCommand, CreateKeyCommand, KMSClient } from "@aws-sdk/client-kms";
import { CreateDBClusterCommand, CreateDBInstanceCommand, CreateDBSubnetGroupCommand, RDSClient } from "@aws-sdk/client-rds";
import { ChangeResourceRecordSetsCommand, CreateHostedZoneCommand, Route53Client } from "@aws-sdk/client-route-53";
import { CreateBucketCommand, PutObjectCommand, S3Client } from "@aws-sdk/client-s3";
import { CreateSecretCommand, SecretsManagerClient } from "@aws-sdk/client-secrets-manager";
import { GetParameterCommand, PutParameterCommand, SSMClient } from "@aws-sdk/client-ssm";
import { GetCallerIdentityCommand, STSClient } from "@aws-sdk/client-sts";
import { CreateWebACLCommand, WAFV2Client } from "@aws-sdk/client-wafv2";
import { NodeHttpHandler } from "@smithy/node-http-handler";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { Agent } from "node:https";
import { join } from "node:path";

/** What one list page must show once the emulator is seeded. */
export interface SeededResource {
  service: string;
  resource: string;
  /** Text the item's row shows, and the link to its detail page. */
  text: string;
  /** Text each child table of the item's detail page shows, by child id. */
  children?: Record<string, string>;
}

export interface SeedTarget {
  endpoint: string;
  ca: Buffer;
  credentials: { accessKeyId: string; secretAccessKey: string };
}

export const SECRET_VALUE = "secret-value-the-console-must-not-show";
export const PARAMETER_VALUE = "parameter-value-the-console-must-not-show";

const REGION = "us-east-1";
const KUBERNETES_VERSION = "1.34";
const AMI_PARAMETER = `/aws/service/eks/optimized-ami/${KUBERNETES_VERSION}/amazon-linux-2023/x86_64/standard/recommended/image_id`;
/** The AWS managed CachingOptimized cache policy. */
const CACHING_OPTIMIZED_POLICY = "658327ea-f89d-4fab-a63d-7e88639e58f6";
const CLUSTER_ACTIVE_DEADLINE_MS = 60_000;

function named(resourceType: TagSpecification["ResourceType"], name: string): TagSpecification[] {
  return [{ ResourceType: resourceType, Tags: [{ Key: "Name", Value: name }] }];
}

function required<T>(value: T | undefined | null, what: string): T {
  if (value === undefined || value === null) {
    throw new Error(`seeding failed: the response had no ${what}`);
  }
  return value;
}

function trust(service: string): string {
  return JSON.stringify({
    Version: "2012-10-17",
    Statement: [{ Effect: "Allow", Principal: { Service: service }, Action: "sts:AssumeRole" }],
  });
}

/** Create one resource of every type the console lists, and say where each shows up. */
export async function seedEverything(target: SeedTarget): Promise<SeededResource[]> {
  const config = {
    endpoint: target.endpoint,
    region: REGION,
    credentials: target.credentials,
    requestHandler: new NodeHttpHandler({ httpsAgent: new Agent({ ca: target.ca }) }),
  };
  const seeded: SeededResource[] = [];
  const add = (entry: SeededResource) => seeded.push(entry);

  const account = required((await new STSClient(config).send(new GetCallerIdentityCommand({}))).Account, "account");

  // IAM
  const iam = new IAMClient(config);
  const clusterRole = required((await iam.send(new CreateRoleCommand({ RoleName: "seed-eks-cluster", AssumeRolePolicyDocument: trust("eks.amazonaws.com") }))).Role?.Arn, "role ARN");
  const nodeRole = required((await iam.send(new CreateRoleCommand({ RoleName: "seed-eks-node", AssumeRolePolicyDocument: trust("ec2.amazonaws.com") }))).Role?.Arn, "role ARN");
  add({ service: "iam", resource: "roles", text: "seed-eks-cluster" });
  await iam.send(new CreateUserCommand({ UserName: "seed-user" }));
  add({ service: "iam", resource: "users", text: "seed-user" });
  await iam.send(
    new CreatePolicyCommand({
      PolicyName: "seed-read-logs",
      PolicyDocument: JSON.stringify({ Version: "2012-10-17", Statement: [{ Effect: "Allow", Action: "logs:Describe*", Resource: "*" }] }),
    }),
  );
  add({ service: "iam", resource: "policies", text: "seed-read-logs" });

  // KMS
  const kms = new KMSClient(config);
  const keyId = required((await kms.send(new CreateKeyCommand({ Description: "seed key" }))).KeyMetadata?.KeyId, "key id");
  add({ service: "kms", resource: "keys", text: keyId });
  await kms.send(new CreateAliasCommand({ AliasName: "alias/seed-key", TargetKeyId: keyId }));
  add({ service: "kms", resource: "aliases", text: "alias/seed-key" });

  // Secrets Manager, Systems Manager
  await new SecretsManagerClient(config).send(new CreateSecretCommand({ Name: "seed/secret", SecretString: SECRET_VALUE }));
  add({ service: "secretsmanager", resource: "secrets", text: "seed/secret" });
  const ssm = new SSMClient(config);
  await ssm.send(new PutParameterCommand({ Name: "/seed/parameter", Value: PARAMETER_VALUE, Type: "String" }));
  add({ service: "ssm", resource: "parameters", text: "/seed/parameter" });

  // ACM, WAF
  await new ACMClient(config).send(new RequestCertificateCommand({ DomainName: "seed.aws-emulator.dev", ValidationMethod: "DNS" }));
  add({ service: "acm", resource: "certificates", text: "seed.aws-emulator.dev" });
  const waf = new WAFV2Client(config);
  for (const [scope, name, resource] of [
    ["REGIONAL", "seed-regional-acl", "regional-web-acls"],
    ["CLOUDFRONT", "seed-cloudfront-acl", "cloudfront-web-acls"],
  ] as const) {
    await waf.send(
      new CreateWebACLCommand({
        Name: name,
        Scope: scope,
        DefaultAction: { Allow: {} },
        Rules: [],
        VisibilityConfig: { SampledRequestsEnabled: false, CloudWatchMetricsEnabled: false, MetricName: name },
      }),
    );
    add({ service: "wafv2", resource, text: name });
  }

  // S3
  const s3 = new S3Client({ ...config, forcePathStyle: true });
  await s3.send(new CreateBucketCommand({ Bucket: "seed-bucket" }));
  await s3.send(new PutObjectCommand({ Bucket: "seed-bucket", Key: "reports/seed.csv", Body: "a,b\n1,2\n" }));
  add({ service: "s3", resource: "buckets", text: "seed-bucket", children: { objects: "reports/seed.csv" } });

  // ECR
  const ecr = new ECRClient(config);
  await ecr.send(new CreateRepositoryCommand({ repositoryName: "seed-repo" }));
  // A manifest is accepted only once the blobs it names are in the repository.
  const uploadBlob = async (content: string) => {
    const blob = new TextEncoder().encode(content);
    const digest = `sha256:${createHash("sha256").update(blob).digest("hex")}`;
    const uploadId = required((await ecr.send(new InitiateLayerUploadCommand({ repositoryName: "seed-repo" }))).uploadId, "upload id");
    await ecr.send(new UploadLayerPartCommand({ repositoryName: "seed-repo", uploadId, partFirstByte: 0, partLastByte: blob.length - 1, layerPartBlob: blob }));
    await ecr.send(new CompleteLayerUploadCommand({ repositoryName: "seed-repo", uploadId, layerDigests: [digest] }));
    return { digest, size: blob.length };
  };
  const imageConfig = await uploadBlob(JSON.stringify({ architecture: "amd64", os: "linux", rootfs: { type: "layers", diff_ids: [] } }));
  const layer = await uploadBlob("seed layer");
  await ecr.send(
    new PutImageCommand({
      repositoryName: "seed-repo",
      imageTag: "v1",
      imageManifestMediaType: "application/vnd.docker.distribution.manifest.v2+json",
      imageManifest: JSON.stringify({
        schemaVersion: 2,
        mediaType: "application/vnd.docker.distribution.manifest.v2+json",
        config: { mediaType: "application/vnd.docker.container.image.v1+json", ...imageConfig },
        layers: [{ mediaType: "application/vnd.docker.image.rootfs.diff.tar.gzip", ...layer }],
      }),
    }),
  );
  add({ service: "ecr", resource: "repositories", text: "seed-repo", children: { images: "v1" } });

  // VPC
  const ec2 = new EC2Client(config);
  const vpcId = required(
    (await ec2.send(new CreateVpcCommand({ CidrBlock: "10.80.0.0/16", TagSpecifications: named("vpc", "seed-vpc") }))).Vpc?.VpcId,
    "VPC id",
  );
  add({ service: "vpc", resource: "vpcs", text: "seed-vpc" });
  const subnets: string[] = [];
  for (const [zone, cidr] of [
    ["us-east-1a", "10.80.1.0/24"],
    ["us-east-1b", "10.80.2.0/24"],
  ] as const) {
    const subnet = await ec2.send(
      new CreateSubnetCommand({ VpcId: vpcId, CidrBlock: cidr, AvailabilityZone: zone, TagSpecifications: named("subnet", `seed-subnet-${zone}`) }),
    );
    subnets.push(required(subnet.Subnet?.SubnetId, "subnet id"));
  }
  add({ service: "vpc", resource: "subnets", text: "seed-subnet-us-east-1a" });
  await ec2.send(new CreateRouteTableCommand({ VpcId: vpcId, TagSpecifications: named("route-table", "seed-route-table") }));
  add({ service: "vpc", resource: "route-tables", text: "seed-route-table" });
  const gateway = required(
    (await ec2.send(new CreateInternetGatewayCommand({ TagSpecifications: named("internet-gateway", "seed-igw") }))).InternetGateway?.InternetGatewayId,
    "internet gateway id",
  );
  await ec2.send(new AttachInternetGatewayCommand({ InternetGatewayId: gateway, VpcId: vpcId }));
  add({ service: "vpc", resource: "internet-gateways", text: "seed-igw" });
  const allocation = required(
    (await ec2.send(new AllocateAddressCommand({ Domain: "vpc", TagSpecifications: named("elastic-ip", "seed-eip") }))).AllocationId,
    "allocation id",
  );
  add({ service: "ec2", resource: "elastic-ips", text: "seed-eip" });
  await ec2.send(
    new CreateNatGatewayCommand({ SubnetId: required(subnets[0], "subnet"), AllocationId: allocation, TagSpecifications: named("natgateway", "seed-nat") }),
  );
  add({ service: "vpc", resource: "nat-gateways", text: "seed-nat" });
  await ec2.send(new CreateNetworkAclCommand({ VpcId: vpcId, TagSpecifications: named("network-acl", "seed-nacl") }));
  add({ service: "vpc", resource: "network-acls", text: "seed-nacl" });
  const securityGroup = required(
    (await ec2.send(new CreateSecurityGroupCommand({ GroupName: "seed-sg", Description: "seed security group", VpcId: vpcId }))).GroupId,
    "security group id",
  );
  add({ service: "ec2", resource: "security-groups", text: "seed-sg" });

  // EC2, Auto Scaling
  const imageId = required((await ssm.send(new GetParameterCommand({ Name: AMI_PARAMETER }))).Parameter?.Value, "AMI id");
  await ec2.send(
    new RunInstancesCommand({
      ImageId: imageId,
      InstanceType: "t3.micro",
      MinCount: 1,
      MaxCount: 1,
      SubnetId: subnets[0],
      TagSpecifications: named("instance", "seed-instance"),
    }),
  );
  add({ service: "ec2", resource: "instances", text: "seed-instance" });
  await ec2.send(
    new CreateLaunchTemplateCommand({ LaunchTemplateName: "seed-launch-template", LaunchTemplateData: { ImageId: imageId, InstanceType: "t3.micro" } }),
  );
  add({ service: "ec2", resource: "launch-templates", text: "seed-launch-template" });
  await new AutoScalingClient(config).send(
    new CreateAutoScalingGroupCommand({
      AutoScalingGroupName: "seed-asg",
      LaunchTemplate: { LaunchTemplateName: "seed-launch-template", Version: "$Latest" },
      MinSize: 0,
      MaxSize: 1,
      DesiredCapacity: 0,
      VPCZoneIdentifier: subnets.join(","),
    }),
  );
  add({ service: "autoscaling", resource: "groups", text: "seed-asg" });

  // EKS: a node group needs an active cluster.
  const eks = new EKSClient(config);
  await eks.send(
    new CreateClusterCommand({ name: "seed-cluster", version: KUBERNETES_VERSION, roleArn: clusterRole, resourcesVpcConfig: { subnetIds: subnets } }),
  );
  const deadline = Date.now() + CLUSTER_ACTIVE_DEADLINE_MS;
  for (;;) {
    const status = (await eks.send(new DescribeClusterCommand({ name: "seed-cluster" }))).cluster?.status;
    if (status === "ACTIVE") {
      break;
    }
    if (Date.now() > deadline) {
      throw new Error(`seeding failed: the EKS cluster is ${status ?? "missing"}, not ACTIVE, after ${CLUSTER_ACTIVE_DEADLINE_MS} ms`);
    }
    await new Promise((resolve) => setTimeout(resolve, 1_000));
  }
  await eks.send(
    new CreateNodegroupCommand({
      clusterName: "seed-cluster",
      nodegroupName: "seed-nodes",
      nodeRole,
      subnets,
      instanceTypes: ["t3.medium"],
      amiType: "AL2023_x86_64_STANDARD",
      scalingConfig: { minSize: 1, maxSize: 1, desiredSize: 1 },
    }),
  );
  add({ service: "eks", resource: "clusters", text: "seed-cluster", children: { nodegroups: "seed-nodes" } });

  // RDS
  const rds = new RDSClient(config);
  await rds.send(new CreateDBSubnetGroupCommand({ DBSubnetGroupName: "seed-db-subnets", DBSubnetGroupDescription: "seed", SubnetIds: subnets }));
  add({ service: "rds", resource: "subnet-groups", text: "seed-db-subnets" });
  await rds.send(
    new CreateDBInstanceCommand({
      DBInstanceIdentifier: "seed-postgres",
      Engine: "postgres",
      EngineVersion: "17",
      DBInstanceClass: "db.t4g.micro",
      AllocatedStorage: 20,
      MasterUsername: "app",
      MasterUserPassword: "seed-password-local-only",
      DBSubnetGroupName: "seed-db-subnets",
    }),
  );
  add({ service: "rds", resource: "instances", text: "seed-postgres" });
  await rds.send(
    new CreateDBClusterCommand({
      DBClusterIdentifier: "seed-aurora",
      Engine: "aurora-postgresql",
      MasterUsername: "app",
      MasterUserPassword: "seed-password-local-only",
      DBSubnetGroupName: "seed-db-subnets",
    }),
  );
  add({ service: "rds", resource: "clusters", text: "seed-aurora" });

  // ElastiCache
  const elastiCache = new ElastiCacheClient(config);
  await elastiCache.send(new CreateCacheSubnetGroupCommand({ CacheSubnetGroupName: "seed-cache-subnets", CacheSubnetGroupDescription: "seed", SubnetIds: subnets }));
  add({ service: "elasticache", resource: "subnet-groups", text: "seed-cache-subnets" });
  await elastiCache.send(
    new CreateReplicationGroupCommand({
      ReplicationGroupId: "seed-valkey",
      ReplicationGroupDescription: "seed",
      Engine: "valkey",
      EngineVersion: "8.0",
      CacheNodeType: "cache.t4g.micro",
      NumCacheClusters: 1,
      CacheSubnetGroupName: "seed-cache-subnets",
    }),
  );
  add({ service: "elasticache", resource: "replication-groups", text: "seed-valkey" });
  await elastiCache.send(
    new CreateCacheClusterCommand({
      CacheClusterId: "seed-memcached",
      Engine: "memcached",
      CacheNodeType: "cache.t4g.micro",
      NumCacheNodes: 1,
      CacheSubnetGroupName: "seed-cache-subnets",
    }),
  );
  add({ service: "elasticache", resource: "cache-clusters", text: "seed-memcached" });

  // Elastic Load Balancing
  const elb = new ElasticLoadBalancingV2Client(config);
  const targetGroup = required(
    (await elb.send(new CreateTargetGroupCommand({ Name: "seed-targets", Protocol: "HTTP", Port: 80, VpcId: vpcId, TargetType: "ip" }))).TargetGroups?.[0]?.TargetGroupArn,
    "target group ARN",
  );
  add({ service: "elbv2", resource: "target-groups", text: "seed-targets" });
  const loadBalancer = required(
    (await elb.send(new CreateLoadBalancerCommand({ Name: "seed-alb", Type: "application", Subnets: subnets, SecurityGroups: [securityGroup] }))).LoadBalancers?.[0]
      ?.LoadBalancerArn,
    "load balancer ARN",
  );
  await elb.send(
    new CreateListenerCommand({ LoadBalancerArn: loadBalancer, Protocol: "HTTP", Port: 80, DefaultActions: [{ Type: "forward", TargetGroupArn: targetGroup }] }),
  );
  add({ service: "elbv2", resource: "load-balancers", text: "seed-alb", children: { listeners: "HTTP" } });

  // CloudFront
  const distribution = await new CloudFrontClient(config).send(
    new CreateDistributionCommand({
      DistributionConfig: {
        CallerReference: "seed-distribution",
        Comment: "seed",
        Enabled: true,
        Origins: {
          Quantity: 1,
          Items: [{ Id: "seed-origin", DomainName: "seed-bucket.s3.us-east-1.amazonaws.com", S3OriginConfig: { OriginAccessIdentity: "" } }],
        },
        DefaultCacheBehavior: { TargetOriginId: "seed-origin", ViewerProtocolPolicy: "redirect-to-https", CachePolicyId: CACHING_OPTIMIZED_POLICY },
      },
    }),
  );
  add({ service: "cloudfront", resource: "distributions", text: required(distribution.Distribution?.Id, "distribution id") });

  // Route 53
  const route53 = new Route53Client(config);
  const zoneId = required(
    (await route53.send(new CreateHostedZoneCommand({ Name: "seed.internal", CallerReference: "seed-zone" }))).HostedZone?.Id,
    "hosted zone id",
  );
  await route53.send(
    new ChangeResourceRecordSetsCommand({
      HostedZoneId: zoneId,
      ChangeBatch: {
        Changes: [{ Action: "UPSERT", ResourceRecordSet: { Name: "app.seed.internal", Type: "A", TTL: 60, ResourceRecords: [{ Value: "10.80.1.10" }] } }],
      },
    }),
  );
  add({ service: "route53", resource: "hosted-zones", text: "seed.internal.", children: { records: "app.seed.internal." } });

  // CloudWatch Logs, CloudWatch
  const logs = new CloudWatchLogsClient(config);
  await logs.send(new CreateLogGroupCommand({ logGroupName: "/seed/app" }));
  await logs.send(new CreateLogStreamCommand({ logGroupName: "/seed/app", logStreamName: "seed-stream" }));
  add({ service: "logs", resource: "log-groups", text: "/seed/app", children: { "log-streams": "seed-stream" } });
  await new CloudWatchClient(config).send(
    new PutMetricAlarmCommand({
      AlarmName: "seed-alarm",
      Namespace: "AWS/EC2",
      MetricName: "CPUUtilization",
      Statistic: "Average",
      Period: 60,
      EvaluationPeriods: 1,
      Threshold: 80,
      ComparisonOperator: "GreaterThanThreshold",
    }),
  );
  add({ service: "cloudwatch", resource: "alarms", text: "seed-alarm" });

  // Budgets
  await new BudgetsClient(config).send(
    new CreateBudgetCommand({
      AccountId: account,
      Budget: { BudgetName: "seed-budget", BudgetType: "COST", TimeUnit: "MONTHLY", BudgetLimit: { Amount: "100", Unit: "USD" } },
    }),
  );
  add({ service: "budgets", resource: "budgets", text: "seed-budget" });

  return seeded;
}

/** The key pair in an emulator's `emulator_credentials` file. */
export function emulatorKeys(folder: string): SeedTarget["credentials"] {
  const text = readFileSync(join(folder, "emulator_credentials"), "utf8");
  const value = (name: string) => {
    const match = new RegExp(`^${name}\\s*=\\s*(\\S+)`, "m").exec(text);
    return required(match?.[1], `${name} in ${folder}/emulator_credentials`);
  };
  return { accessKeyId: value("aws_access_key_id"), secretAccessKey: value("aws_secret_access_key") };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const [endpoint, folder] = process.argv.slice(2);
  if (!endpoint || !folder) {
    console.error("usage: node --experimental-strip-types e2e/seed.ts <endpoint> <emulator folder, e.g. ~/.aws/emulator/9297>");
    process.exit(2);
  }
  const seeded = await seedEverything({ endpoint, ca: readFileSync(join(folder, "ca.pem")), credentials: emulatorKeys(folder) });
  console.log(`seeded ${seeded.length} resource types`);
}
