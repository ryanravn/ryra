import { test, expect } from "@playwright/test";

const AUTHELIA_USER = process.env.AUTHELIA_USER || "admin";
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

  // 2. Wait for Authelia's login form
  const signInBtn = page.getByRole("button", { name: /sign in/i });
  await expect(signInBtn).toBeVisible({ timeout: 15_000 });

  // 3. Fill credentials and submit
  await page.locator("#username-textfield").fill(AUTHELIA_USER);
  await page.locator("#password-textfield").fill(AUTHELIA_PASSWORD);
  await signInBtn.click();

  // 4. After login, Authelia should redirect back to whoami through Caddy
  await page.waitForURL((url) => url.toString().includes("whoami.test.local"), {
    timeout: 15_000,
  });

  // 5. Verify whoami responded — it echoes request headers
  const body = await page.locator("body").textContent();
  expect(body).toContain("Hostname:");
});
