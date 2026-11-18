import { describe, expect, it } from "vitest";
import { CredentialsFileError, loadSession, parseCredentialsFile, saveSession } from "./session";

describe("parseCredentialsFile", () => {
  it("reads the key pair the emulator writes", () => {
    const text = "[default]\naws_access_key_id = AKIAEXAMPLE\naws_secret_access_key = secret/with=equals\n";
    expect(parseCredentialsFile(text)).toEqual({ accessKeyId: "AKIAEXAMPLE", secretAccessKey: "secret/with=equals" });
  });

  it("prefers the default profile when there are several", () => {
    const text = "[other]\naws_access_key_id = A\naws_secret_access_key = B\n[default]\naws_access_key_id = C\naws_secret_access_key = D\n";
    expect(parseCredentialsFile(text).accessKeyId).toBe("C");
  });

  it("reads the only profile when it is not named default", () => {
    expect(parseCredentialsFile("[emulator]\naws_access_key_id = A\naws_secret_access_key = B").accessKeyId).toBe("A");
  });

  it("refuses text with no profile", () => {
    expect(() => parseCredentialsFile("aws_access_key_id = A")).toThrow(CredentialsFileError);
  });

  it("refuses a profile without both keys", () => {
    expect(() => parseCredentialsFile("[default]\naws_access_key_id = A")).toThrow(/aws_secret_access_key/);
  });
});

function memoryStorage() {
  const values = new Map<string, string>();
  return {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => void values.set(key, value),
    removeItem: (key: string) => void values.delete(key),
  };
}

describe("the tab session", () => {
  it("returns the session it saved", () => {
    const storage = memoryStorage();
    const session = { accessKeyId: "A", secretAccessKey: "B", region: "eu-west-1" };
    saveSession(session, storage);
    expect(loadSession(storage)).toEqual(session);
  });

  it("forgets the session when it is cleared", () => {
    const storage = memoryStorage();
    saveSession({ accessKeyId: "A", secretAccessKey: "B", region: "us-east-1" }, storage);
    saveSession(undefined, storage);
    expect(loadSession(storage)).toBeUndefined();
  });

  it("ignores a stored value that is not a session", () => {
    const storage = memoryStorage();
    storage.setItem("aws-emulator-console-session", '{"accessKeyId":1}');
    expect(loadSession(storage)).toBeUndefined();
  });

  it("asks again when storage throws", () => {
    const blocked = {
      getItem: () => {
        throw new Error("blocked");
      },
    };
    expect(loadSession(blocked)).toBeUndefined();
  });
});
