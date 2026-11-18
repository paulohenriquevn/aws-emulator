import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

/** The emulator `npm run dev` forwards the console's AWS API calls to. */
const EMULATOR_URL = process.env.EMULATOR_URL ?? "https://localhost:9297";

// The emulator serves the built files under /_console/ (fork/console/src/lib.rs).
export default defineConfig({
  base: "/_console/",
  plugins: [react()],
  build: { outDir: "dist", emptyOutDir: true, chunkSizeWarningLimit: 4096 },
  server: {
    proxy: {
      // Every path outside the app is an AWS API call. The Host header is kept, because
      // the SDK signed the request for the dev server's host.
      "^/(?!_console(/|$)).*": { target: EMULATOR_URL, secure: false, changeOrigin: false },
    },
  },
  test: { include: ["src/**/*.test.ts"] },
});
