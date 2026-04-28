import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: ".",
  timeout: 30_000,
  retries: 0,
  use: {
    baseURL: `http://127.0.0.1:${process.env.PORT_HTTP || "2283"}`,
    headless: true,
  },
});
