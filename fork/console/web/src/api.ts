/** What every console API call needs: where the emulator is, and who signs. */

import type { Session } from "./session";

export interface ApiContext {
  /** The emulator endpoint, which is the origin the console is served from. */
  endpoint: string;
  region: string;
  credentials: { accessKeyId: string; secretAccessKey: string };
}

/** Global services (IAM, Route 53, CloudFront, Budgets) sign in this region. */
export const GLOBAL_REGION = "us-east-1";

export function apiContext(session: Session, endpoint: string = globalThis.location.origin): ApiContext {
  return {
    endpoint,
    region: session.region,
    credentials: { accessKeyId: session.accessKeyId, secretAccessKey: session.secretAccessKey },
  };
}

/** The constructor options of an AWS SDK client that talks to the emulator. */
export function clientConfig(context: ApiContext, scope: "regional" | "global" = "regional") {
  return {
    endpoint: context.endpoint,
    region: scope === "global" ? GLOBAL_REGION : context.region,
    credentials: context.credentials,
    // One attempt: a failure is shown as it happened, not after the SDK's retries.
    maxAttempts: 1,
  };
}

/** Every item a paginated SDK operation returns, across all pages. */
export async function collect<Page, Entry>(pages: AsyncIterable<Page>, pick: (page: Page) => Entry[] | undefined): Promise<Entry[]> {
  const entries: Entry[] = [];
  for await (const page of pages) {
    entries.push(...(pick(page) ?? []));
  }
  return entries;
}

/** The name and message of an error an SDK call threw, for the page to show. */
export function describeError(error: unknown): { title: string; message: string } {
  if (error instanceof Error) {
    return { title: error.name || "Error", message: error.message || "The call failed with no message." };
  }
  return { title: "Error", message: String(error) };
}
