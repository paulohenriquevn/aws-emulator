/**
 * Every service and resource type the console lists, and the read-only AWS API calls
 * that list them. Each SDK client is imported on first use, so a page only downloads
 * the clients it calls.
 */

import { type ApiContext, clientConfig, collect } from "./api";
import { display, epochMillis, type Item, nameTag, valueAt } from "./format";

export interface Column {
  header: string;
  cell: (item: Item) => string;
}

/** Resources that live under one item of a resource type, such as the objects of a bucket. */
export interface ChildResource {
  id: string;
  title: string;
  list: (context: ApiContext, parent: Item) => Promise<Item[]>;
  key: (item: Item) => string;
  columns: Column[];
}

export interface ResourceType {
  /** The path segment of the resource's pages, unique within its service. */
  id: string;
  title: string;
  list: (context: ApiContext) => Promise<Item[]>;
  /** What identifies one item in the URL of its detail page. */
  key: (item: Item) => string;
  columns: Column[];
  children?: ChildResource[];
}

export interface Service {
  /** The path segment of the service's pages. */
  id: string;
  name: string;
  category: string;
  resources: ResourceType[];
}

/** A column showing the value at `path`, formatted by `format`. */
function column(header: string, path: string, format: (value: unknown) => string = display): Column {
  return { header, cell: (item) => format(valueAt(item, path)) };
}

/** A column showing the `Name` tag, read from `path` (`Tags` unless given). */
function nameColumn(path = "Tags"): Column {
  return { header: "Name", cell: (item) => display(nameTag(valueAt(item, path))) };
}

/** The key function for items identified by the value at `path`. */
function keyAt(path: string): (item: Item) => string {
  return (item) => {
    const value = valueAt(item, path);
    if (value === undefined || value === null) {
      throw new Error(`an item has no ${path}: ${JSON.stringify(item)}`);
    }
    return String(value);
  };
}

const items = (entries: object[] | undefined): Item[] => (entries ?? []) as Item[];

const iam: Service = {
  id: "iam",
  name: "IAM",
  category: "Security, identity and compliance",
  resources: [
    {
      id: "roles",
      title: "Roles",
      key: keyAt("RoleName"),
      columns: [column("Role name", "RoleName"), column("ARN", "Arn"), column("Created", "CreateDate")],
      list: async (context) => {
        const { IAMClient, paginateListRoles } = await import("@aws-sdk/client-iam");
        const client = new IAMClient(clientConfig(context, "global"));
        return items(await collect(paginateListRoles({ client }, {}), (page) => page.Roles));
      },
    },
    {
      id: "users",
      title: "Users",
      key: keyAt("UserName"),
      columns: [column("User name", "UserName"), column("ARN", "Arn"), column("Created", "CreateDate")],
      list: async (context) => {
        const { IAMClient, paginateListUsers } = await import("@aws-sdk/client-iam");
        const client = new IAMClient(clientConfig(context, "global"));
        return items(await collect(paginateListUsers({ client }, {}), (page) => page.Users));
      },
    },
    {
      id: "policies",
      title: "Customer managed policies",
      key: keyAt("PolicyName"),
      columns: [column("Policy name", "PolicyName"), column("ARN", "Arn"), column("Attachments", "AttachmentCount")],
      list: async (context) => {
        const { IAMClient, paginateListPolicies } = await import("@aws-sdk/client-iam");
        const client = new IAMClient(clientConfig(context, "global"));
        return items(await collect(paginateListPolicies({ client }, { Scope: "Local" }), (page) => page.Policies));
      },
    },
  ],
};

const kms: Service = {
  id: "kms",
  name: "Key Management Service",
  category: "Security, identity and compliance",
  resources: [
    {
      id: "keys",
      title: "Keys",
      key: keyAt("KeyId"),
      columns: [
        column("Key ID", "KeyId"),
        column("State", "KeyState"),
        column("Usage", "KeyUsage"),
        column("Description", "Description"),
        column("Created", "CreationDate"),
      ],
      list: async (context) => {
        const { DescribeKeyCommand, KMSClient, paginateListKeys } = await import("@aws-sdk/client-kms");
        const client = new KMSClient(clientConfig(context));
        const keys = await collect(paginateListKeys({ client }, {}), (page) => page.Keys);
        const described = await Promise.all(
          keys.map(async (key) => (await client.send(new DescribeKeyCommand({ KeyId: key.KeyId }))).KeyMetadata),
        );
        return items(described.filter((metadata) => metadata !== undefined));
      },
    },
    {
      id: "aliases",
      title: "Aliases",
      key: keyAt("AliasName"),
      columns: [column("Alias", "AliasName"), column("Target key", "TargetKeyId"), column("ARN", "AliasArn")],
      list: async (context) => {
        const { KMSClient, paginateListAliases } = await import("@aws-sdk/client-kms");
        const client = new KMSClient(clientConfig(context));
        return items(await collect(paginateListAliases({ client }, {}), (page) => page.Aliases));
      },
    },
  ],
};

const secretsManager: Service = {
  id: "secretsmanager",
  name: "Secrets Manager",
  category: "Security, identity and compliance",
  resources: [
    {
      // Only the metadata ListSecrets returns: the console never reads a secret value.
      id: "secrets",
      title: "Secrets",
      key: keyAt("Name"),
      columns: [column("Secret name", "Name"), column("Description", "Description"), column("Last changed", "LastChangedDate")],
      list: async (context) => {
        const { SecretsManagerClient, paginateListSecrets } = await import("@aws-sdk/client-secrets-manager");
        const client = new SecretsManagerClient(clientConfig(context));
        return items(await collect(paginateListSecrets({ client }, {}), (page) => page.SecretList));
      },
    },
  ],
};

const acm: Service = {
  id: "acm",
  name: "Certificate Manager",
  category: "Security, identity and compliance",
  resources: [
    {
      id: "certificates",
      title: "Certificates",
      key: keyAt("CertificateArn"),
      columns: [column("Domain name", "DomainName"), column("Status", "Status"), column("Type", "Type"), column("ARN", "CertificateArn")],
      list: async (context) => {
        const { ACMClient, paginateListCertificates } = await import("@aws-sdk/client-acm");
        const client = new ACMClient(clientConfig(context));
        return items(await collect(paginateListCertificates({ client }, {}), (page) => page.CertificateSummaryList));
      },
    },
  ],
};

/** WAF lists web ACLs one scope at a time and pages them with NextMarker. */
async function listWebAcls(context: ApiContext, scope: "REGIONAL" | "CLOUDFRONT"): Promise<Item[]> {
  const { ListWebACLsCommand, WAFV2Client } = await import("@aws-sdk/client-wafv2");
  const client = new WAFV2Client(clientConfig(context, scope === "CLOUDFRONT" ? "global" : "regional"));
  const acls: Item[] = [];
  let marker: string | undefined;
  do {
    const page = await client.send(new ListWebACLsCommand({ Scope: scope, NextMarker: marker }));
    acls.push(...items(page.WebACLs));
    marker = page.NextMarker;
  } while (marker);
  return acls;
}

const wafv2: Service = {
  id: "wafv2",
  name: "WAF",
  category: "Security, identity and compliance",
  resources: [
    {
      id: "regional-web-acls",
      title: "Web ACLs (regional)",
      key: keyAt("Id"),
      columns: [column("Name", "Name"), column("ID", "Id"), column("Description", "Description")],
      list: (context) => listWebAcls(context, "REGIONAL"),
    },
    {
      id: "cloudfront-web-acls",
      title: "Web ACLs (CloudFront)",
      key: keyAt("Id"),
      columns: [column("Name", "Name"), column("ID", "Id"), column("Description", "Description")],
      list: (context) => listWebAcls(context, "CLOUDFRONT"),
    },
  ],
};

const s3: Service = {
  id: "s3",
  name: "S3",
  category: "Storage",
  resources: [
    {
      id: "buckets",
      title: "Buckets",
      key: keyAt("Name"),
      columns: [column("Bucket name", "Name"), column("Region", "BucketRegion"), column("Created", "CreationDate")],
      list: async (context) => {
        const { S3Client, paginateListBuckets } = await import("@aws-sdk/client-s3");
        const client = new S3Client({ ...clientConfig(context), forcePathStyle: true });
        return items(await collect(paginateListBuckets({ client }, {}), (page) => page.Buckets));
      },
      children: [
        {
          id: "objects",
          title: "Objects",
          key: keyAt("Key"),
          columns: [column("Key", "Key"), column("Size (bytes)", "Size"), column("Last modified", "LastModified")],
          list: async (context, bucket) => {
            const { S3Client, paginateListObjectsV2 } = await import("@aws-sdk/client-s3");
            const client = new S3Client({ ...clientConfig(context), forcePathStyle: true });
            return items(
              await collect(paginateListObjectsV2({ client }, { Bucket: String(bucket.Name) }), (page) => page.Contents),
            );
          },
        },
      ],
    },
  ],
};

const ecr: Service = {
  id: "ecr",
  name: "Elastic Container Registry",
  category: "Containers",
  resources: [
    {
      id: "repositories",
      title: "Repositories",
      key: keyAt("repositoryName"),
      columns: [column("Repository name", "repositoryName"), column("URI", "repositoryUri"), column("Created", "createdAt")],
      list: async (context) => {
        const { ECRClient, paginateDescribeRepositories } = await import("@aws-sdk/client-ecr");
        const client = new ECRClient(clientConfig(context));
        return items(await collect(paginateDescribeRepositories({ client }, {}), (page) => page.repositories));
      },
      children: [
        {
          id: "images",
          title: "Images",
          key: keyAt("imageDigest"),
          columns: [column("Digest", "imageDigest"), column("Tags", "imageTags"), column("Pushed", "imagePushedAt")],
          list: async (context, repository) => {
            const { ECRClient, paginateDescribeImages } = await import("@aws-sdk/client-ecr");
            const client = new ECRClient(clientConfig(context));
            return items(
              await collect(
                paginateDescribeImages({ client }, { repositoryName: String(repository.repositoryName) }),
                (page) => page.imageDetails,
              ),
            );
          },
        },
      ],
    },
  ],
};

const eks: Service = {
  id: "eks",
  name: "Elastic Kubernetes Service",
  category: "Containers",
  resources: [
    {
      id: "clusters",
      title: "Clusters",
      key: keyAt("name"),
      columns: [column("Cluster name", "name"), column("Status", "status"), column("Kubernetes version", "version"), column("Endpoint", "endpoint")],
      list: async (context) => {
        const { DescribeClusterCommand, EKSClient, paginateListClusters } = await import("@aws-sdk/client-eks");
        const client = new EKSClient(clientConfig(context));
        const names = await collect(paginateListClusters({ client }, {}), (page) => page.clusters);
        const clusters = await Promise.all(
          names.map(async (name) => (await client.send(new DescribeClusterCommand({ name }))).cluster),
        );
        return items(clusters.filter((cluster) => cluster !== undefined));
      },
      children: [
        {
          id: "nodegroups",
          title: "Node groups",
          key: keyAt("nodegroupName"),
          columns: [column("Node group", "nodegroupName"), column("Status", "status"), column("Instance types", "instanceTypes")],
          list: async (context, cluster) => {
            const { DescribeNodegroupCommand, EKSClient, paginateListNodegroups } = await import("@aws-sdk/client-eks");
            const client = new EKSClient(clientConfig(context));
            const clusterName = String(cluster.name);
            const names = await collect(paginateListNodegroups({ client }, { clusterName }), (page) => page.nodegroups);
            const nodegroups = await Promise.all(
              names.map(
                async (nodegroupName) =>
                  (await client.send(new DescribeNodegroupCommand({ clusterName, nodegroupName }))).nodegroup,
              ),
            );
            return items(nodegroups.filter((nodegroup) => nodegroup !== undefined));
          },
        },
      ],
    },
  ],
};

async function ec2Client(context: ApiContext) {
  const sdk = await import("@aws-sdk/client-ec2");
  return { sdk, client: new sdk.EC2Client(clientConfig(context)) };
}

const ec2: Service = {
  id: "ec2",
  name: "EC2",
  category: "Compute",
  resources: [
    {
      id: "instances",
      title: "Instances",
      key: keyAt("InstanceId"),
      columns: [
        column("Instance ID", "InstanceId"),
        nameColumn(),
        column("State", "State.Name"),
        column("Type", "InstanceType"),
        column("Private IP", "PrivateIpAddress"),
      ],
      list: async (context) => {
        const { sdk, client } = await ec2Client(context);
        const reservations = await collect(sdk.paginateDescribeInstances({ client }, {}), (page) => page.Reservations);
        return items(reservations.flatMap((reservation) => reservation.Instances ?? []));
      },
    },
    {
      id: "launch-templates",
      title: "Launch templates",
      key: keyAt("LaunchTemplateId"),
      columns: [column("Launch template ID", "LaunchTemplateId"), column("Name", "LaunchTemplateName"), column("Latest version", "LatestVersionNumber")],
      list: async (context) => {
        const { sdk, client } = await ec2Client(context);
        return items(await collect(sdk.paginateDescribeLaunchTemplates({ client }, {}), (page) => page.LaunchTemplates));
      },
    },
    {
      id: "security-groups",
      title: "Security groups",
      key: keyAt("GroupId"),
      columns: [column("Group ID", "GroupId"), column("Group name", "GroupName"), column("VPC", "VpcId"), column("Description", "Description")],
      list: async (context) => {
        const { sdk, client } = await ec2Client(context);
        return items(await collect(sdk.paginateDescribeSecurityGroups({ client }, {}), (page) => page.SecurityGroups));
      },
    },
    {
      id: "elastic-ips",
      title: "Elastic IPs",
      key: keyAt("AllocationId"),
      columns: [column("Allocation ID", "AllocationId"), nameColumn(), column("Public IP", "PublicIp"), column("Association", "AssociationId")],
      list: async (context) => {
        const { sdk, client } = await ec2Client(context);
        return items((await client.send(new sdk.DescribeAddressesCommand({}))).Addresses);
      },
    },
  ],
};

const vpc: Service = {
  id: "vpc",
  name: "VPC",
  category: "Networking and content delivery",
  resources: [
    {
      id: "vpcs",
      title: "VPCs",
      key: keyAt("VpcId"),
      columns: [column("VPC ID", "VpcId"), nameColumn(), column("CIDR", "CidrBlock"), column("State", "State"), column("Default", "IsDefault")],
      list: async (context) => {
        const { sdk, client } = await ec2Client(context);
        return items(await collect(sdk.paginateDescribeVpcs({ client }, {}), (page) => page.Vpcs));
      },
    },
    {
      id: "subnets",
      title: "Subnets",
      key: keyAt("SubnetId"),
      columns: [column("Subnet ID", "SubnetId"), nameColumn(), column("VPC", "VpcId"), column("CIDR", "CidrBlock"), column("Availability Zone", "AvailabilityZone")],
      list: async (context) => {
        const { sdk, client } = await ec2Client(context);
        return items(await collect(sdk.paginateDescribeSubnets({ client }, {}), (page) => page.Subnets));
      },
    },
    {
      id: "route-tables",
      title: "Route tables",
      key: keyAt("RouteTableId"),
      columns: [
        column("Route table ID", "RouteTableId"),
        nameColumn(),
        column("VPC", "VpcId"),
        { header: "Routes", cell: (item) => display((valueAt(item, "Routes") as unknown[] | undefined)?.length) },
      ],
      list: async (context) => {
        const { sdk, client } = await ec2Client(context);
        return items(await collect(sdk.paginateDescribeRouteTables({ client }, {}), (page) => page.RouteTables));
      },
    },
    {
      id: "internet-gateways",
      title: "Internet gateways",
      key: keyAt("InternetGatewayId"),
      columns: [
        column("Internet gateway ID", "InternetGatewayId"),
        nameColumn(),
        { header: "Attached VPCs", cell: (item) => display((valueAt(item, "Attachments") as Item[] | undefined)?.map((a) => a.VpcId)) },
      ],
      list: async (context) => {
        const { sdk, client } = await ec2Client(context);
        return items(await collect(sdk.paginateDescribeInternetGateways({ client }, {}), (page) => page.InternetGateways));
      },
    },
    {
      id: "nat-gateways",
      title: "NAT gateways",
      key: keyAt("NatGatewayId"),
      columns: [column("NAT gateway ID", "NatGatewayId"), nameColumn(), column("State", "State"), column("Subnet", "SubnetId"), column("VPC", "VpcId")],
      list: async (context) => {
        const { sdk, client } = await ec2Client(context);
        return items(await collect(sdk.paginateDescribeNatGateways({ client }, {}), (page) => page.NatGateways));
      },
    },
    {
      id: "network-acls",
      title: "Network ACLs",
      key: keyAt("NetworkAclId"),
      columns: [column("Network ACL ID", "NetworkAclId"), nameColumn(), column("VPC", "VpcId"), column("Default", "IsDefault")],
      list: async (context) => {
        const { sdk, client } = await ec2Client(context);
        return items(await collect(sdk.paginateDescribeNetworkAcls({ client }, {}), (page) => page.NetworkAcls));
      },
    },
  ],
};

const elb: Service = {
  id: "elbv2",
  name: "Elastic Load Balancing",
  category: "Networking and content delivery",
  resources: [
    {
      id: "load-balancers",
      title: "Load balancers",
      key: keyAt("LoadBalancerName"),
      columns: [column("Name", "LoadBalancerName"), column("Type", "Type"), column("State", "State.Code"), column("DNS name", "DNSName")],
      list: async (context) => {
        const { ElasticLoadBalancingV2Client, paginateDescribeLoadBalancers } = await import("@aws-sdk/client-elastic-load-balancing-v2");
        const client = new ElasticLoadBalancingV2Client(clientConfig(context));
        return items(await collect(paginateDescribeLoadBalancers({ client }, {}), (page) => page.LoadBalancers));
      },
      children: [
        {
          id: "listeners",
          title: "Listeners",
          key: keyAt("ListenerArn"),
          columns: [column("Protocol", "Protocol"), column("Port", "Port"), column("ARN", "ListenerArn")],
          list: async (context, loadBalancer) => {
            const { ElasticLoadBalancingV2Client, paginateDescribeListeners } = await import("@aws-sdk/client-elastic-load-balancing-v2");
            const client = new ElasticLoadBalancingV2Client(clientConfig(context));
            return items(
              await collect(
                paginateDescribeListeners({ client }, { LoadBalancerArn: String(loadBalancer.LoadBalancerArn) }),
                (page) => page.Listeners,
              ),
            );
          },
        },
      ],
    },
    {
      id: "target-groups",
      title: "Target groups",
      key: keyAt("TargetGroupName"),
      columns: [column("Name", "TargetGroupName"), column("Protocol", "Protocol"), column("Port", "Port"), column("Target type", "TargetType"), column("VPC", "VpcId")],
      list: async (context) => {
        const { ElasticLoadBalancingV2Client, paginateDescribeTargetGroups } = await import("@aws-sdk/client-elastic-load-balancing-v2");
        const client = new ElasticLoadBalancingV2Client(clientConfig(context));
        return items(await collect(paginateDescribeTargetGroups({ client }, {}), (page) => page.TargetGroups));
      },
    },
  ],
};

const autoScaling: Service = {
  id: "autoscaling",
  name: "Auto Scaling",
  category: "Compute",
  resources: [
    {
      id: "groups",
      title: "Auto Scaling groups",
      key: keyAt("AutoScalingGroupName"),
      columns: [
        column("Name", "AutoScalingGroupName"),
        column("Desired", "DesiredCapacity"),
        column("Min", "MinSize"),
        column("Max", "MaxSize"),
        column("Created", "CreatedTime"),
      ],
      list: async (context) => {
        const { AutoScalingClient, paginateDescribeAutoScalingGroups } = await import("@aws-sdk/client-auto-scaling");
        const client = new AutoScalingClient(clientConfig(context));
        return items(await collect(paginateDescribeAutoScalingGroups({ client }, {}), (page) => page.AutoScalingGroups));
      },
    },
  ],
};

const cloudFront: Service = {
  id: "cloudfront",
  name: "CloudFront",
  category: "Networking and content delivery",
  resources: [
    {
      id: "distributions",
      title: "Distributions",
      key: keyAt("Id"),
      columns: [column("ID", "Id"), column("Domain name", "DomainName"), column("Status", "Status"), column("Enabled", "Enabled"), column("Aliases", "Aliases.Items")],
      list: async (context) => {
        const { CloudFrontClient, paginateListDistributions } = await import("@aws-sdk/client-cloudfront");
        const client = new CloudFrontClient(clientConfig(context, "global"));
        return items(await collect(paginateListDistributions({ client }, {}), (page) => page.DistributionList?.Items));
      },
    },
  ],
};

const route53: Service = {
  id: "route53",
  name: "Route 53",
  category: "Networking and content delivery",
  resources: [
    {
      id: "hosted-zones",
      title: "Hosted zones",
      key: keyAt("Id"),
      columns: [column("Domain name", "Name"), column("ID", "Id"), column("Records", "ResourceRecordSetCount"), column("Private", "Config.PrivateZone")],
      list: async (context) => {
        const { Route53Client, paginateListHostedZones } = await import("@aws-sdk/client-route-53");
        const client = new Route53Client(clientConfig(context, "global"));
        return items(await collect(paginateListHostedZones({ client }, {}), (page) => page.HostedZones));
      },
      children: [
        {
          id: "records",
          title: "Records",
          key: (record) => `${String(record.Name)} ${String(record.Type)} ${display(record.SetIdentifier)}`,
          columns: [
            column("Record name", "Name"),
            column("Type", "Type"),
            column("TTL", "TTL"),
            {
              header: "Value",
              cell: (record) =>
                display(
                  valueAt(record, "AliasTarget.DNSName") ??
                    (valueAt(record, "ResourceRecords") as Item[] | undefined)?.map((value) => value.Value),
                ),
            },
          ],
          list: async (context, zone) => {
            const { ListResourceRecordSetsCommand, Route53Client } = await import("@aws-sdk/client-route-53");
            type Input = import("@aws-sdk/client-route-53").ListResourceRecordSetsCommandInput;
            const client = new Route53Client(clientConfig(context, "global"));
            // ListResourceRecordSets pages by the name and type of the next record, not by a token.
            const records: Item[] = [];
            let next: Pick<Input, "StartRecordName" | "StartRecordType" | "StartRecordIdentifier"> = {};
            for (;;) {
              const page = await client.send(new ListResourceRecordSetsCommand({ HostedZoneId: String(zone.Id), ...next }));
              records.push(...items(page.ResourceRecordSets));
              if (!page.IsTruncated) {
                return records;
              }
              next = {
                StartRecordName: page.NextRecordName,
                StartRecordType: page.NextRecordType,
                StartRecordIdentifier: page.NextRecordIdentifier,
              };
            }
          },
        },
      ],
    },
  ],
};

const rds: Service = {
  id: "rds",
  name: "RDS",
  category: "Database",
  resources: [
    {
      id: "instances",
      title: "DB instances",
      key: keyAt("DBInstanceIdentifier"),
      columns: [
        column("DB identifier", "DBInstanceIdentifier"),
        column("Engine", "Engine"),
        column("Status", "DBInstanceStatus"),
        column("Class", "DBInstanceClass"),
        { header: "Endpoint", cell: (item) => (valueAt(item, "Endpoint.Address") ? `${display(valueAt(item, "Endpoint.Address"))}:${display(valueAt(item, "Endpoint.Port"))}` : "-") },
      ],
      list: async (context) => {
        const { RDSClient, paginateDescribeDBInstances } = await import("@aws-sdk/client-rds");
        const client = new RDSClient(clientConfig(context));
        return items(await collect(paginateDescribeDBInstances({ client }, {}), (page) => page.DBInstances));
      },
    },
    {
      id: "clusters",
      title: "DB clusters",
      key: keyAt("DBClusterIdentifier"),
      columns: [column("DB cluster", "DBClusterIdentifier"), column("Engine", "Engine"), column("Status", "Status"), column("Endpoint", "Endpoint")],
      list: async (context) => {
        const { RDSClient, paginateDescribeDBClusters } = await import("@aws-sdk/client-rds");
        const client = new RDSClient(clientConfig(context));
        return items(await collect(paginateDescribeDBClusters({ client }, {}), (page) => page.DBClusters));
      },
    },
    {
      id: "subnet-groups",
      title: "Subnet groups",
      key: keyAt("DBSubnetGroupName"),
      columns: [column("Name", "DBSubnetGroupName"), column("VPC", "VpcId"), column("Status", "SubnetGroupStatus"), column("Description", "DBSubnetGroupDescription")],
      list: async (context) => {
        const { RDSClient, paginateDescribeDBSubnetGroups } = await import("@aws-sdk/client-rds");
        const client = new RDSClient(clientConfig(context));
        return items(await collect(paginateDescribeDBSubnetGroups({ client }, {}), (page) => page.DBSubnetGroups));
      },
    },
  ],
};

const elastiCache: Service = {
  id: "elasticache",
  name: "ElastiCache",
  category: "Database",
  resources: [
    {
      id: "replication-groups",
      title: "Replication groups",
      key: keyAt("ReplicationGroupId"),
      columns: [column("Replication group", "ReplicationGroupId"), column("Status", "Status"), column("Description", "Description"), column("Node type", "CacheNodeType")],
      list: async (context) => {
        const { ElastiCacheClient, paginateDescribeReplicationGroups } = await import("@aws-sdk/client-elasticache");
        const client = new ElastiCacheClient(clientConfig(context));
        return items(await collect(paginateDescribeReplicationGroups({ client }, {}), (page) => page.ReplicationGroups));
      },
    },
    {
      id: "cache-clusters",
      title: "Cache clusters",
      key: keyAt("CacheClusterId"),
      columns: [column("Cluster", "CacheClusterId"), column("Engine", "Engine"), column("Status", "CacheClusterStatus"), column("Node type", "CacheNodeType")],
      list: async (context) => {
        const { ElastiCacheClient, paginateDescribeCacheClusters } = await import("@aws-sdk/client-elasticache");
        const client = new ElastiCacheClient(clientConfig(context));
        return items(await collect(paginateDescribeCacheClusters({ client }, {}), (page) => page.CacheClusters));
      },
    },
    {
      id: "subnet-groups",
      title: "Subnet groups",
      key: keyAt("CacheSubnetGroupName"),
      columns: [column("Name", "CacheSubnetGroupName"), column("VPC", "VpcId"), column("Description", "CacheSubnetGroupDescription")],
      list: async (context) => {
        const { ElastiCacheClient, paginateDescribeCacheSubnetGroups } = await import("@aws-sdk/client-elasticache");
        const client = new ElastiCacheClient(clientConfig(context));
        return items(await collect(paginateDescribeCacheSubnetGroups({ client }, {}), (page) => page.CacheSubnetGroups));
      },
    },
  ],
};

const ssm: Service = {
  id: "ssm",
  name: "Systems Manager",
  category: "Management and governance",
  resources: [
    {
      // DescribeParameters returns metadata only: the console never reads a parameter value.
      id: "parameters",
      title: "Parameters",
      key: keyAt("Name"),
      columns: [column("Name", "Name"), column("Type", "Type"), column("Version", "Version"), column("Last modified", "LastModifiedDate")],
      list: async (context) => {
        const { SSMClient, paginateDescribeParameters } = await import("@aws-sdk/client-ssm");
        const client = new SSMClient(clientConfig(context));
        return items(await collect(paginateDescribeParameters({ client }, {}), (page) => page.Parameters));
      },
    },
  ],
};

const cloudWatchLogs: Service = {
  id: "logs",
  name: "CloudWatch Logs",
  category: "Management and governance",
  resources: [
    {
      id: "log-groups",
      title: "Log groups",
      key: keyAt("logGroupName"),
      columns: [
        column("Log group", "logGroupName"),
        column("Retention (days)", "retentionInDays"),
        column("Stored bytes", "storedBytes"),
        column("Created", "creationTime", epochMillis),
      ],
      list: async (context) => {
        const { CloudWatchLogsClient, paginateDescribeLogGroups } = await import("@aws-sdk/client-cloudwatch-logs");
        const client = new CloudWatchLogsClient(clientConfig(context));
        return items(await collect(paginateDescribeLogGroups({ client }, {}), (page) => page.logGroups));
      },
      children: [
        {
          id: "log-streams",
          title: "Log streams",
          key: keyAt("logStreamName"),
          columns: [column("Log stream", "logStreamName"), column("Created", "creationTime", epochMillis), column("Last event", "lastEventTimestamp", epochMillis)],
          list: async (context, group) => {
            const { CloudWatchLogsClient, paginateDescribeLogStreams } = await import("@aws-sdk/client-cloudwatch-logs");
            const client = new CloudWatchLogsClient(clientConfig(context));
            return items(
              await collect(
                paginateDescribeLogStreams({ client }, { logGroupName: String(group.logGroupName) }),
                (page) => page.logStreams,
              ),
            );
          },
        },
      ],
    },
  ],
};

const cloudWatch: Service = {
  id: "cloudwatch",
  name: "CloudWatch",
  category: "Management and governance",
  resources: [
    {
      id: "alarms",
      title: "Metric alarms",
      key: keyAt("AlarmName"),
      columns: [column("Alarm name", "AlarmName"), column("State", "StateValue"), column("Metric", "MetricName"), column("Namespace", "Namespace")],
      list: async (context) => {
        const { CloudWatchClient, paginateDescribeAlarms } = await import("@aws-sdk/client-cloudwatch");
        const client = new CloudWatchClient(clientConfig(context));
        return items(await collect(paginateDescribeAlarms({ client }, {}), (page) => page.MetricAlarms));
      },
    },
  ],
};

const budgets: Service = {
  id: "budgets",
  name: "Budgets",
  category: "Cloud financial management",
  resources: [
    {
      id: "budgets",
      title: "Budgets",
      key: keyAt("BudgetName"),
      columns: [
        column("Budget name", "BudgetName"),
        column("Type", "BudgetType"),
        column("Period", "TimeUnit"),
        { header: "Limit", cell: (item) => (valueAt(item, "BudgetLimit.Amount") ? `${display(valueAt(item, "BudgetLimit.Amount"))} ${display(valueAt(item, "BudgetLimit.Unit"))}` : "-") },
      ],
      list: async (context) => {
        const { BudgetsClient, paginateDescribeBudgets } = await import("@aws-sdk/client-budgets");
        const client = new BudgetsClient(clientConfig(context, "global"));
        const AccountId = await accountId(context);
        return items(await collect(paginateDescribeBudgets({ client }, { AccountId }), (page) => page.Budgets));
      },
    },
  ],
};

const cloudFormation: Service = {
  id: "cloudformation",
  name: "CloudFormation",
  category: "Management and governance",
  resources: [
    {
      id: "stacks",
      title: "Stacks",
      key: keyAt("StackName"),
      columns: [column("Stack name", "StackName"), column("Status", "StackStatus"), column("Created", "CreationTime"), column("Description", "Description")],
      list: async (context) => {
        const { CloudFormationClient, paginateDescribeStacks } = await import("@aws-sdk/client-cloudformation");
        const client = new CloudFormationClient(clientConfig(context));
        return items(await collect(paginateDescribeStacks({ client }, {}), (page) => page.Stacks));
      },
      children: [
        {
          id: "resources",
          title: "Resources",
          key: keyAt("LogicalResourceId"),
          columns: [
            column("Logical ID", "LogicalResourceId"),
            column("Physical ID", "PhysicalResourceId"),
            column("Type", "ResourceType"),
            column("Status", "ResourceStatus"),
          ],
          list: async (context, stack) => {
            const { CloudFormationClient, paginateListStackResources } = await import("@aws-sdk/client-cloudformation");
            const client = new CloudFormationClient(clientConfig(context));
            return items(
              await collect(paginateListStackResources({ client }, { StackName: String(stack.StackName) }), (page) => page.StackResourceSummaries),
            );
          },
        },
      ],
    },
  ],
};

const sqs: Service = {
  id: "sqs",
  name: "SQS",
  category: "Application integration",
  resources: [
    {
      // ListQueues answers URLs only; the name is the URL's last path segment.
      id: "queues",
      title: "Queues",
      key: keyAt("QueueName"),
      columns: [column("Queue name", "QueueName"), column("URL", "QueueUrl")],
      list: async (context) => {
        const { SQSClient, paginateListQueues } = await import("@aws-sdk/client-sqs");
        const client = new SQSClient(clientConfig(context));
        const urls = await collect(paginateListQueues({ client }, {}), (page) => page.QueueUrls);
        return urls.map((url) => ({ QueueName: url.slice(url.lastIndexOf("/") + 1), QueueUrl: url }));
      },
    },
  ],
};

const eventBridge: Service = {
  id: "events",
  name: "EventBridge",
  category: "Application integration",
  resources: [
    {
      id: "rules",
      title: "Rules on the default bus",
      key: keyAt("Name"),
      columns: [column("Rule name", "Name"), column("State", "State"), column("Event pattern", "EventPattern"), column("Schedule", "ScheduleExpression")],
      list: async (context) => {
        // The EventBridge client ships no paginator for ListRules, so the pages are walked here.
        const { EventBridgeClient, ListRulesCommand } = await import("@aws-sdk/client-eventbridge");
        const client = new EventBridgeClient(clientConfig(context));
        const rules: object[] = [];
        let NextToken: string | undefined;
        do {
          const page = await client.send(new ListRulesCommand({ NextToken }));
          rules.push(...(page.Rules ?? []));
          NextToken = page.NextToken;
        } while (NextToken);
        return items(rules);
      },
    },
  ],
};

/** The account the signing keys belong to, which Budgets needs on every call. */
export async function accountId(context: ApiContext): Promise<string> {
  const identity = await callerIdentity(context);
  if (!identity.Account) {
    throw new Error("GetCallerIdentity returned no Account");
  }
  return identity.Account;
}

/** Who the signing keys are, as STS reports it. */
export async function callerIdentity(context: ApiContext): Promise<{ Account?: string; Arn?: string; UserId?: string }> {
  const { GetCallerIdentityCommand, STSClient } = await import("@aws-sdk/client-sts");
  return new STSClient(clientConfig(context)).send(new GetCallerIdentityCommand({}));
}

/** Every service the console lists, in the order the navigation shows them. */
export const SERVICES: Service[] = [
  ec2,
  autoScaling,
  eks,
  ecr,
  s3,
  rds,
  elastiCache,
  vpc,
  elb,
  cloudFront,
  route53,
  iam,
  kms,
  secretsManager,
  acm,
  wafv2,
  ssm,
  cloudFormation,
  sqs,
  eventBridge,
  cloudWatch,
  cloudWatchLogs,
  budgets,
];

export function findService(serviceId: string | undefined): Service | undefined {
  return SERVICES.find((service) => service.id === serviceId);
}

export function findResource(service: Service | undefined, resourceId: string | undefined): ResourceType | undefined {
  return service?.resources.find((resource) => resource.id === resourceId);
}

/** The services grouped by category, in the order the categories first appear. */
export function servicesByCategory(services: Service[] = SERVICES): Map<string, Service[]> {
  const groups = new Map<string, Service[]>();
  for (const service of services) {
    groups.set(service.category, [...(groups.get(service.category) ?? []), service]);
  }
  return groups;
}
