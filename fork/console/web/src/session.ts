/** The keys and region the console signs its AWS API calls with. */

export interface Session {
  accessKeyId: string;
  secretAccessKey: string;
  region: string;
}

export const DEFAULT_REGION = "us-east-1";

/** Regions offered in the region selector. The emulator accepts any region name. */
export const REGIONS = [
  "us-east-1",
  "us-east-2",
  "us-west-1",
  "us-west-2",
  "ca-central-1",
  "sa-east-1",
  "eu-west-1",
  "eu-west-2",
  "eu-central-1",
  "ap-southeast-1",
  "ap-southeast-2",
  "ap-northeast-1",
];

const STORAGE_KEY = "aws-emulator-console-session";

export class CredentialsFileError extends Error {
  override name = "CredentialsFileError";
}

/**
 * The key pair in an `emulator_credentials` file (or any shared credentials file),
 * taken from its `[default]` profile, or from the only profile when there is one.
 */
export function parseCredentialsFile(text: string): Pick<Session, "accessKeyId" | "secretAccessKey"> {
  const profiles = new Map<string, Map<string, string>>();
  let current: Map<string, string> | undefined;
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#") || line.startsWith(";")) {
      continue;
    }
    const section = /^\[(.+)\]$/.exec(line);
    if (section?.[1] !== undefined) {
      current = new Map();
      profiles.set(section[1].trim(), current);
      continue;
    }
    const separator = line.indexOf("=");
    if (separator > 0 && current !== undefined) {
      current.set(line.slice(0, separator).trim().toLowerCase(), line.slice(separator + 1).trim());
    }
  }
  const profile = profiles.get("default") ?? (profiles.size === 1 ? [...profiles.values()][0] : undefined);
  if (profile === undefined) {
    throw new CredentialsFileError(
      `expected a [default] profile or exactly one profile; found ${profiles.size === 0 ? "none" : [...profiles.keys()].join(", ")}`,
    );
  }
  const accessKeyId = profile.get("aws_access_key_id");
  const secretAccessKey = profile.get("aws_secret_access_key");
  if (!accessKeyId || !secretAccessKey) {
    throw new CredentialsFileError("the profile has no aws_access_key_id and aws_secret_access_key");
  }
  return { accessKeyId, secretAccessKey };
}

/** The session kept for this browser tab, if any. */
export function loadSession(storage: Pick<Storage, "getItem"> | undefined = tabStorage()): Session | undefined {
  try {
    const raw = storage?.getItem(STORAGE_KEY);
    if (!raw) {
      return undefined;
    }
    const parsed = JSON.parse(raw) as Partial<Session>;
    if (typeof parsed.accessKeyId === "string" && typeof parsed.secretAccessKey === "string" && typeof parsed.region === "string") {
      return { accessKeyId: parsed.accessKeyId, secretAccessKey: parsed.secretAccessKey, region: parsed.region };
    }
    return undefined;
  } catch {
    // Storage blocked or holding something else: the console asks for keys again.
    return undefined;
  }
}

/** Keep `session` for this tab only, so closing it forgets the keys. */
export function saveSession(session: Session | undefined, storage: Pick<Storage, "setItem" | "removeItem"> | undefined = tabStorage()): void {
  try {
    if (session === undefined) {
      storage?.removeItem(STORAGE_KEY);
    } else {
      storage?.setItem(STORAGE_KEY, JSON.stringify(session));
    }
  } catch {
    // Storage blocked: the session still lives in memory until the page reloads.
  }
}

function tabStorage(): Storage | undefined {
  try {
    return globalThis.sessionStorage;
  } catch {
    return undefined;
  }
}
