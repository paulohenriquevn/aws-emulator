/**
 * The console against a real emulator: resources created through the AWS SDK show up
 * in the pages a person opens, and the console never shows a secret value.
 */

import { CreateRepositoryCommand, ECRClient } from "@aws-sdk/client-ecr";
import { CreateTagsCommand, CreateVpcCommand, EC2Client } from "@aws-sdk/client-ec2";
import { CreateRoleCommand, IAMClient } from "@aws-sdk/client-iam";
import { CloudWatchLogsClient, CreateLogGroupCommand } from "@aws-sdk/client-cloudwatch-logs";
import { CreateBucketCommand, PutObjectCommand, S3Client } from "@aws-sdk/client-s3";
import { CreateSecretCommand, SecretsManagerClient } from "@aws-sdk/client-secrets-manager";
import { PutParameterCommand, SSMClient } from "@aws-sdk/client-ssm";
import { expect, type Page, test } from "@playwright/test";
import { readFileSync } from "node:fs";
import { SERVICES } from "../src/catalog";
import { parseCredentialsFile } from "../src/session";

const BUCKET = "console-e2e-bucket";
const OBJECT_KEY = "reports/2026.csv";
const VPC_NAME = "console-e2e-vpc";
const PARAMETER = "/console-e2e/flag";
const PARAMETER_VALUE = "parameter-value-the-console-must-not-show";
const SECRET = "console-e2e-secret";
const SECRET_VALUE = "secret-value-the-console-must-not-show";
const LOG_GROUP = "/console-e2e/app";
const REPOSITORY = "console-e2e-repo";
const ROLE = "console-e2e-role";

function keys() {
  const file = process.env.CONSOLE_E2E_CREDENTIALS;
  if (!file) {
    throw new Error("CONSOLE_E2E_CREDENTIALS is unset: run the tests through playwright.config.ts");
  }
  return parseCredentialsFile(readFileSync(file, "utf8"));
}

function sdkConfig() {
  return { endpoint: process.env.CONSOLE_E2E_ENDPOINT, region: "us-east-1", credentials: keys() };
}

let vpcId = "";

/**
 * Create a resource the tests need, accepting one an earlier worker already created:
 * Playwright starts a new worker, and so runs beforeAll again, after a failed test.
 */
async function ensure(create: () => Promise<unknown>): Promise<void> {
  try {
    await create();
  } catch (error) {
    const name = error instanceof Error ? error.name : "";
    if (!/AlreadyExists|AlreadyOwnedByYou|ResourceExistsException/.test(name)) {
      throw error;
    }
  }
}

test.beforeAll(async () => {
  const config = sdkConfig();
  const s3 = new S3Client({ ...config, forcePathStyle: true });
  await ensure(() => s3.send(new CreateBucketCommand({ Bucket: BUCKET })));
  await s3.send(new PutObjectCommand({ Bucket: BUCKET, Key: OBJECT_KEY, Body: "a,b\n1,2\n" }));

  const ec2 = new EC2Client(config);
  const vpc = await ec2.send(new CreateVpcCommand({ CidrBlock: "10.42.0.0/16" }));
  vpcId = vpc.Vpc?.VpcId ?? "";
  expect(vpcId, "CreateVpc returned a VPC id").not.toBe("");
  await ec2.send(new CreateTagsCommand({ Resources: [vpcId], Tags: [{ Key: "Name", Value: VPC_NAME }] }));

  await new SSMClient(config).send(new PutParameterCommand({ Name: PARAMETER, Value: PARAMETER_VALUE, Type: "String", Overwrite: true }));
  await ensure(() => new SecretsManagerClient(config).send(new CreateSecretCommand({ Name: SECRET, SecretString: SECRET_VALUE })));
  await ensure(() => new CloudWatchLogsClient(config).send(new CreateLogGroupCommand({ logGroupName: LOG_GROUP })));
  await ensure(() => new ECRClient(config).send(new CreateRepositoryCommand({ repositoryName: REPOSITORY })));
  const trustPolicy = {
    Version: "2012-10-17",
    Statement: [{ Effect: "Allow", Principal: { Service: "ec2.amazonaws.com" }, Action: "sts:AssumeRole" }],
  };
  await ensure(() =>
    new IAMClient(config).send(new CreateRoleCommand({ RoleName: ROLE, AssumeRolePolicyDocument: JSON.stringify(trustPolicy) })),
  );
});

async function fillSignIn(page: Page, credentials: { accessKeyId: string; secretAccessKey: string }) {
  await page.goto("./");
  await page.getByLabel("Access key ID").fill(credentials.accessKeyId);
  await page.getByLabel("Secret access key").fill(credentials.secretAccessKey);
  await page.getByRole("button", { name: "Sign in" }).click();
}

/** Sign in with the emulator's keys and wait until the console home shows. */
async function signIn(page: Page) {
  await fillSignIn(page, keys());
  await expect(page.getByRole("heading", { name: "Console home" })).toBeVisible();
}

/** Wait until a list page's table has loaded or failed, and fail with the error it shows. */
async function expectListLoaded(page: Page) {
  const counter = page.getByRole("heading", { level: 1, name: /\(\d+\)$/ });
  const failure = page.getByText(/failed:/);
  await expect(counter.or(failure).first()).toBeVisible();
  if ((await failure.count()) > 0) {
    throw new Error(`the page shows an error: ${await page.locator("body").innerText()}`);
  }
}

test("keys the emulator did not issue are refused at sign in", async ({ page }) => {
  await fillSignIn(page, { accessKeyId: "AKIANOTTHEEMULATORS", secretAccessKey: "wrong" });

  await expect(page.getByText(/Sign in failed/)).toBeVisible();
  await expect(page.getByRole("button", { name: "Sign in" })).toBeVisible();
});

test("the home page counts the buckets the emulator holds", async ({ page }) => {
  await signIn(page);

  await expect(page.getByTestId("count-s3-buckets")).toHaveText(/^[1-9]\d*$/);
});

test("a bucket's page lists the objects in it", async ({ page }) => {
  await signIn(page);
  await page.goto("./services/s3/buckets");
  await page.getByRole("link", { name: BUCKET }).click();

  await expect(page.getByRole("heading", { name: BUCKET, level: 1 })).toBeVisible();
  await expect(page.getByRole("cell", { name: OBJECT_KEY })).toBeVisible();
});

test("the VPC list shows a VPC with its Name tag", async ({ page }) => {
  await signIn(page);
  await page.goto("./services/vpc/vpcs");

  const row = page.getByRole("row").filter({ hasText: vpcId });
  await expect(row).toContainText(VPC_NAME);
});

test("the resource lists show what was created through the SDK", async ({ page }) => {
  await signIn(page);
  for (const [path, name] of [
    ["services/logs/log-groups", LOG_GROUP],
    ["services/ecr/repositories", REPOSITORY],
    ["services/iam/roles", ROLE],
  ] as const) {
    await page.goto(`./${path}`);
    await expect(page.getByRole("link", { name }), path).toBeVisible();
  }
});

test("a secret's pages show its name but never its value", async ({ page }) => {
  await signIn(page);
  await page.goto("./services/secretsmanager/secrets");
  await page.getByRole("link", { name: SECRET }).click();

  await expect(page.getByTestId("attributes")).toContainText(SECRET);
  await expect(page.locator("body")).not.toContainText(SECRET_VALUE);
});

test("a parameter's pages show its name but never its value", async ({ page }) => {
  await signIn(page);
  await page.goto("./services/ssm/parameters");
  await page.getByRole("link", { name: PARAMETER }).click();

  await expect(page.getByTestId("attributes")).toContainText(PARAMETER);
  await expect(page.locator("body")).not.toContainText(PARAMETER_VALUE);
});

// Every list call the console makes is one the emulator answers.
for (const service of SERVICES) {
  for (const resource of service.resources) {
    test(`the ${service.name} ${resource.title} page loads without an error`, async ({ page }) => {
      await signIn(page);
      await page.goto(`./services/${service.id}/${resource.id}`);

      await expectListLoaded(page);
    });
  }
}
