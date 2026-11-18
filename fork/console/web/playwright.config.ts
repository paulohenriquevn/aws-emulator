import { defineConfig } from "@playwright/test";

/** The port the end-to-end emulator listens on (e2e/emulator.ts starts it). */
export const E2E_PORT = Number(process.env.CONSOLE_E2E_PORT ?? 9497);

// End-to-end tests against a real emulator binary, named by EMULATOR_BIN.
export default defineConfig({
  testDir: "e2e",
  globalSetup: "./e2e/emulator.ts",
  timeout: 60_000,
  retries: 0,
  workers: 1,
  reporter: [["list"]],
  use: {
    baseURL: `https://localhost:${E2E_PORT}/_console/`,
    // The emulator's certificate comes from its own CA, which the browser does not trust.
    ignoreHTTPSErrors: true,
    trace: "retain-on-failure",
  },
  projects: [{ name: "chromium", use: { browserName: "chromium" } }],
});
