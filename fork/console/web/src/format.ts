/** How the console turns an AWS API value into the text a table cell shows. */

export type Item = Record<string, unknown>;

/** The text shown for a value the API did not return. */
export const ABSENT = "-";

/** The value at a dotted path such as `State.Name`, or undefined when any step is missing. */
export function valueAt(item: unknown, path: string): unknown {
  let current: unknown = item;
  for (const step of path.split(".")) {
    if (current === null || typeof current !== "object") {
      return undefined;
    }
    current = (current as Record<string, unknown>)[step];
  }
  return current;
}

/** A value as one line of text: dates in ISO 8601, lists joined, objects as JSON. */
export function display(value: unknown): string {
  if (value === undefined || value === null || value === "") {
    return ABSENT;
  }
  if (value instanceof Date) {
    return Number.isNaN(value.getTime()) ? ABSENT : value.toISOString();
  }
  if (Array.isArray(value)) {
    return value.length === 0 ? ABSENT : value.map(display).join(", ");
  }
  if (typeof value === "object") {
    return JSON.stringify(value);
  }
  return String(value);
}

/** A time the API reports in epoch milliseconds (CloudWatch Logs does), in ISO 8601. */
export function epochMillis(value: unknown): string {
  return typeof value === "number" ? display(new Date(value)) : ABSENT;
}

/** The value of the `Name` tag, from either tag shape AWS APIs use. */
export function nameTag(tags: unknown): string | undefined {
  if (!Array.isArray(tags)) {
    return undefined;
  }
  for (const tag of tags) {
    if (tag === null || typeof tag !== "object") {
      continue;
    }
    const record = tag as Record<string, unknown>;
    const key = record.Key ?? record.key;
    const value = record.Value ?? record.value;
    if (key === "Name" && typeof value === "string") {
      return value;
    }
  }
  return undefined;
}

/** An item as indented JSON for the detail page, with dates in ISO 8601. */
export function prettyJson(item: unknown): string {
  return JSON.stringify(item, (_key, value: unknown) => (value instanceof Date ? value.toISOString() : value), 2);
}
