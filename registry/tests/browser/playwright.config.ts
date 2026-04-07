import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: ".",
  timeout: 30_000,
  retries: 0,
  use: {
    headless: true,
    // Accept self-signed certs from Caddy's internal TLS
    ignoreHTTPSErrors: true,
  },
});
