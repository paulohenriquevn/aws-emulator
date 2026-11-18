/**
 * Starts the emulator binary named by EMULATOR_BIN in a fresh home folder, waits until
 * it answers its health check, and hands the test workers its endpoint, CA and keys.
 */

import { type ChildProcess, spawn } from "node:child_process";
import { createWriteStream, existsSync, readFileSync } from "node:fs";
import { mkdir, mkdtemp } from "node:fs/promises";
import { request } from "node:https";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { E2E_PORT } from "../playwright.config";

const STARTUP_DEADLINE_MS = 60_000;

function healthy(endpoint: string, ca: Buffer): Promise<boolean> {
  return new Promise((resolve) => {
    const probe = request(`${endpoint}/__admin/health`, { ca, timeout: 2_000 }, (response) => {
      response.resume();
      resolve(response.statusCode === 200);
    });
    probe.on("error", () => resolve(false));
    probe.on("timeout", () => probe.destroy());
    probe.end();
  });
}

export default async function startEmulator(): Promise<() => Promise<void>> {
  const binary = process.env.EMULATOR_BIN;
  if (!binary) {
    throw new Error("EMULATOR_BIN must name the emulator binary to test, e.g. target/debug/aws-emulator-fork");
  }
  // CONSOLE_E2E_HOME keeps the emulator's home (and its log) where CI can upload it.
  const home = process.env.CONSOLE_E2E_HOME ?? (await mkdtemp(join(tmpdir(), "console-e2e-")));
  await mkdir(home, { recursive: true });
  const log = join(home, "emulator.log");
  const emulator: ChildProcess = spawn(binary, ["-p", String(E2E_PORT), "--telemetry", "off"], {
    env: { ...process.env, HOME: home },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const logStream = createWriteStream(log);
  emulator.stdout?.pipe(logStream);
  emulator.stderr?.pipe(logStream);

  const folder = join(home, ".aws", "emulator", String(E2E_PORT));
  const caFile = join(folder, "ca.pem");
  const endpoint = `https://localhost:${E2E_PORT}`;
  const deadline = Date.now() + STARTUP_DEADLINE_MS;
  for (;;) {
    if (emulator.exitCode !== null) {
      throw new Error(`the emulator exited with ${emulator.exitCode} before it was healthy; log: ${log}`);
    }
    if (existsSync(caFile) && (await healthy(endpoint, readFileSync(caFile)))) {
      break;
    }
    if (Date.now() > deadline) {
      emulator.kill();
      throw new Error(`the emulator was not healthy on ${endpoint} within ${STARTUP_DEADLINE_MS} ms; log: ${log}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }

  // Workers start after this returns, so they inherit these.
  process.env.CONSOLE_E2E_ENDPOINT = endpoint;
  process.env.CONSOLE_E2E_CREDENTIALS = join(folder, "emulator_credentials");
  process.env.NODE_EXTRA_CA_CERTS = caFile;
  process.env.CONSOLE_E2E_LOG = log;

  return async () => {
    emulator.kill();
    await new Promise((resolve) => emulator.once("exit", resolve));
  };
}
