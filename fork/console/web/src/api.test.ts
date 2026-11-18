import { describe, expect, it } from "vitest";
import { apiContext, clientConfig, collect, describeError, GLOBAL_REGION } from "./api";

const context = apiContext({ accessKeyId: "A", secretAccessKey: "B", region: "eu-west-1" }, "https://localhost:9297");

describe("clientConfig", () => {
  it("points a regional client at the emulator in the session region", () => {
    expect(clientConfig(context)).toMatchObject({ endpoint: "https://localhost:9297", region: "eu-west-1", maxAttempts: 1 });
  });

  it("signs a global service in us-east-1 whatever the session region", () => {
    expect(clientConfig(context, "global").region).toBe(GLOBAL_REGION);
  });
});

describe("collect", () => {
  it("joins the entries of every page", async () => {
    async function* pages() {
      yield { Items: [1, 2] };
      yield { Items: undefined };
      yield { Items: [3] };
    }
    expect(await collect(pages(), (page) => page.Items)).toEqual([1, 2, 3]);
  });
});

describe("describeError", () => {
  it("uses the name and message of an SDK error", () => {
    const error = Object.assign(new Error("Sign the request with the emulator credentials"), { name: "AccessDenied" });
    expect(describeError(error)).toEqual({ title: "AccessDenied", message: "Sign the request with the emulator credentials" });
  });

  it("describes a thrown value that is not an Error", () => {
    expect(describeError("boom")).toEqual({ title: "Error", message: "boom" });
  });
});
