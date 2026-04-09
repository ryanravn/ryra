import { test, expect } from "@playwright/test";

const AUTHELIA_USER = process.env.AUTHELIA_USER || "testuser";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

// Access whoami through Caddy (HTTPS with forward auth)
const WHOAMI_CADDY_URL = "https://whoami.test.local:8443";

test("unauthenticated request is blocked by forward auth", async ({
  page,
}) => {
  await page.goto(WHOAMI_CADDY_URL, { timeout: 15_000 });

  const url = page.url();
  const body = (await page.locator("body").textContent()) ?? "";
  const blocked =
    url.includes("auth.test.local") ||
    body.includes("Sign in") ||
    body.includes("Unauthorized");
  expect(blocked).toBe(true);
});

test("login through Authelia grants access to forward-auth-protected service", async ({
  page,
}) => {
  // 1. Try to access whoami through Caddy — should redirect to Authelia
  await page.goto(WHOAMI_CADDY_URL, { timeout: 15_000 });

  // 2. Wait for Authelia's React app to fully hydrate before interacting
  await page.waitForLoadState("networkidle");
  const usernameInput = page.locator("#username-textfield");
  await expect(usernameInput).toBeVisible({ timeout: 15_000 });
  await expect(usernameInput).toBeEditable({ timeout: 5_000 });

  // 3. Fill credentials and submit
  await usernameInput.fill(AUTHELIA_USER);
  await page.locator("#password-textfield").fill(AUTHELIA_PASSWORD);
  await page.getByRole("button", { name: /sign in/i }).click();

  // 4. After login, Authelia should redirect back to whoami through Caddy
  //    (check hostname, not full URL — the rd= query param on the Authelia page also contains whoami.test.local)
  await page.waitForURL((url) => new URL(url.toString()).hostname === "whoami.test.local", {
    timeout: 15_000,
  });

  // 5. Verify whoami responded — it echoes request headers
  const body = await page.locator("body").textContent();
  expect(body).toContain("Hostname:");
});
