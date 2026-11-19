import { describe, expect, it } from "vitest";
import { ABSENT, display, epochMillis, nameTag, prettyJson, valueAt } from "./format";

describe("valueAt", () => {
  it("reads a dotted path through nested objects", () => {
    expect(valueAt({ State: { Name: "running" } }, "State.Name")).toBe("running");
  });

  it("is undefined when a step of the path is missing", () => {
    expect(valueAt({ State: null }, "State.Name")).toBeUndefined();
  });
});

describe("display", () => {
  it("shows a missing or empty value as the absent marker", () => {
    expect([display(undefined), display(null), display(""), display([])]).toEqual([ABSENT, ABSENT, ABSENT, ABSENT]);
  });

  it("shows a date in ISO 8601", () => {
    expect(display(new Date(Date.UTC(2026, 10, 18, 12, 0, 0)))).toBe("2026-11-18T12:00:00.000Z");
  });

  it("joins a list with commas", () => {
    expect(display(["t3.micro", "t3.small"])).toBe("t3.micro, t3.small");
  });

  it("keeps false and zero, which are values and not absences", () => {
    expect([display(false), display(0)]).toEqual(["false", "0"]);
  });
});

describe("epochMillis", () => {
  it("turns epoch milliseconds into ISO 8601", () => {
    expect(epochMillis(0)).toBe("1970-01-01T00:00:00.000Z");
  });

  it("shows a value that is not a number as absent", () => {
    expect(epochMillis("soon")).toBe(ABSENT);
  });
});

describe("nameTag", () => {
  it("reads the Name tag in the Key and Value shape", () => {
    expect(nameTag([{ Key: "env", Value: "dev" }, { Key: "Name", Value: "main" }])).toBe("main");
  });

  it("reads the Name tag in the key and value shape", () => {
    expect(nameTag([{ key: "Name", value: "main" }])).toBe("main");
  });

  it("is undefined when no tag is named Name", () => {
    expect(nameTag([{ Key: "env", Value: "dev" }])).toBeUndefined();
  });
});

describe("prettyJson", () => {
  it("writes dates in ISO 8601", () => {
    expect(prettyJson({ at: new Date(0) })).toContain('"at": "1970-01-01T00:00:00.000Z"');
  });
});
