import { test, expect } from "@playwright/test";

const AUTHELIA_USER = process.env.AUTHELIA_USER || "testuser";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

// With `--auth` + auto-HTTPS promotion, paperless-ngx lives behind Caddy at
// https://paperless-ngx.internal:<caddy_https_port> (default 8443).
const PAPERLESS_URL = process.env.PAPERLESS_URL || "https://paperless-ngx.internal:8443";

/** Fill in Authelia's login form and submit. */
async function loginToAuthelia(page: import("@playwright/test").Page) {
  await page.waitForLoadState("networkidle");
  await page.waitForFunction(
    () => {
      const root = document.getElementById("root");
      return root && (Object.keys(root).some(k => k.startsWith("__react")) || (root as any)._reactRootContainer);
    },
    { timeout: 10_000 },
  ).catch(() => {});
  const usernameInput = page.locator("#username-textfield");
  await expect(usernameInput).toBeVisible({ timeout: 15_000 });
  await expect(usernameInput).toBeEditable({ timeout: 5_000 });

  await usernameInput.fill(AUTHELIA_USER);
  await page.locator("#password-textfield").fill(AUTHELIA_PASSWORD);
  await page.getByRole("button", { name: /sign in/i }).click();

  // Accept consent screen if shown
  try {
    const consent = page.getByRole("button", { name: /accept|consent|allow|approve/i });
    await consent.click({ timeout: 10_000 });
  } catch {
    // No consent screen
  }
}

test("full OIDC login through Authelia creates a session", async ({ browser }) => {
  // Authelia uses HTTPS (self-signed cert), so ignore HTTPS errors.
  const context = await browser.newContext({
    ignoreHTTPSErrors: true,
    viewport: { width: 1280, height: 1600 },
  });
  const page = await context.newPage();

  // 1. Go to Paperless-ngx login page
  await page.goto(PAPERLESS_URL, { timeout: 15_000 });
  await page.waitForLoadState("networkidle");

  // 2. Click SSO button (it's a form POST, not a link)
  const ssoButton = page.locator("button:has-text('SSO')");
  await expect(ssoButton).toBeVisible({ timeout: 15_000 });
  await ssoButton.click();

  // 3. Should redirect to Authelia
  await page.waitForURL(
    (url) => url.hostname === "auth.internal",
    { timeout: 15_000 },
  );

  // 4. Login at Authelia
  await loginToAuthelia(page);

  // 5. Should redirect back to Paperless-ngx (signup page or dashboard)
  await page.waitForURL(
    (url) => url.hostname === "paperless-ngx.internal" && !url.pathname.includes("/login/callback/"),
    { timeout: 15_000 },
  );

  // 6. Verify we're past the login page — either on signup confirmation or dashboard
  await page.waitForLoadState("networkidle");
  const url = page.url();
  expect(url).not.toContain("/accounts/login");

  await context.close();
});
