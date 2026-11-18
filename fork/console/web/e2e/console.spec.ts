/**
 * The console against a real emulator seeded with one resource of every type it lists
 * (e2e/seed.ts): each list page shows its resource, each detail page opens with its
 * child resources, and no page shows a secret or parameter value.
 */

import { expect, type Page, test } from "@playwright/test";
import { readFileSync } from "node:fs";
import { SERVICES } from "../src/catalog";
import { parseCredentialsFile } from "../src/session";
import { PARAMETER_VALUE, SECRET_VALUE, type SeededResource } from "./seed";

function fromEnvironment(name: string): string {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${name} is unset: run the tests through playwright.config.ts`);
  }
  return value;
}

const keys = () => parseCredentialsFile(readFileSync(fromEnvironment("CONSOLE_E2E_CREDENTIALS"), "utf8"));
const seeded: SeededResource[] = JSON.parse(readFileSync(fromEnvironment("CONSOLE_E2E_SEEDED"), "utf8"));

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

/** Fail with the page's error when it shows one, so a failed call names its cause. */
async function expectNoError(page: Page) {
  const failure = page.getByText(/failed:/);
  if ((await failure.count()) > 0) {
    throw new Error(`the page shows an error: ${await failure.first().innerText()}`);
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

for (const [service, resource, value] of [
  ["secretsmanager", "secrets", SECRET_VALUE],
  ["ssm", "parameters", PARAMETER_VALUE],
] as const) {
  test(`the ${service} ${resource} pages never show the stored value`, async ({ page }) => {
    const entry = seeded.find((candidate) => candidate.service === service && candidate.resource === resource);
    await signIn(page);
    await page.goto(`./services/${service}/${resource}`);
    await page.getByRole("link", { name: entry?.text, exact: true }).click();

    await expect(page.getByTestId("attributes")).toContainText(entry?.text ?? "");
    await expect(page.locator("body")).not.toContainText(value);
  });
}

// One test per resource type in the catalog, so a type added without a seed fails here.
for (const service of SERVICES) {
  for (const resource of service.resources) {
    test(`${service.name} ${resource.title}: the seeded item is listed and its page opens`, async ({ page }) => {
      const entry = seeded.find((candidate) => candidate.service === service.id && candidate.resource === resource.id);
      expect(entry, `e2e/seed.ts creates no ${service.id}/${resource.id}`).toBeDefined();
      await signIn(page);
      await page.goto(`./services/${service.id}/${resource.id}`);

      const row = page.getByRole("row").filter({ hasText: entry?.text });
      await expect(row.first()).toBeVisible().catch(async (error: unknown) => {
        await expectNoError(page);
        throw error;
      });
      await row.first().getByRole("link").first().click();

      await expect(page.getByTestId("attributes")).toBeVisible();
      await expectNoError(page);
      for (const child of resource.children ?? []) {
        const text = entry?.children?.[child.id];
        expect(text, `e2e/seed.ts creates no ${child.id} under ${service.id}/${resource.id}`).toBeDefined();
        await expect(page.getByRole("table", { name: child.title }).getByText(text ?? "", { exact: true }).first()).toBeVisible();
      }
    });
  }
}
