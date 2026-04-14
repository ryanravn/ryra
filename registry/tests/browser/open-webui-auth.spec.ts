import { test, expect } from "@playwright/test";

const AUTHELIA_USER = process.env.AUTHELIA_USER || "testuser";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

const OPEN_WEBUI_PORT = process.env.OPEN_WEBUI_PORT || "3000";
const OPEN_WEBUI_URL = `http://127.0.0.1:${OPEN_WEBUI_PORT}`;

/** Fill in Authelia's login form and submit. */
async function loginToAuthelia(page: import("@playwright/test").Page) {
  await page.waitForLoadState("networkidle");
  await page.waitForFunction(
    () => {
      const root = document.getElementById("root");
      return root && (Object.keys(root).some(k => k.startsWith("__react")) || (root as any)._reactRootContainer);
    },
    { timeout: 10_000 },
  ).catch(() => {
    // Fallback: if React detection fails, just wait a bit
  });
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
    // No consent screen — already authorized or auto-consented
  }
}

test("open-webui login page loads", async ({ page }) => {
  await page.goto(OPEN_WEBUI_URL, { timeout: 15_000 });
  await page.waitForLoadState("networkidle");
  // Open WebUI shows a login/signup page
  const heading = page.locator("text=/sign in|log in|get started/i").first();
  await expect(heading).toBeVisible({ timeout: 15_000 });
});

test("open-webui shows SSO button when OIDC is configured", async ({
  page,
}) => {
  await page.goto(OPEN_WEBUI_URL, { timeout: 15_000 });
  await page.waitForLoadState("networkidle");
  // Open WebUI shows an SSO button with the configured OAUTH_PROVIDER_NAME
  const ssoButton = page.locator("button:has-text('SSO')");
  await expect(ssoButton).toBeVisible({ timeout: 15_000 });
});

test("clicking SSO button initiates OIDC flow", async ({ browser }) => {
  // Authelia uses HTTPS (self-signed cert), so ignore HTTPS errors.
  const context = await browser.newContext({
    ignoreHTTPSErrors: true,
    viewport: { width: 1280, height: 1600 },
  });
  const page = await context.newPage();
  await page.goto(OPEN_WEBUI_URL, { timeout: 15_000 });
  await page.waitForLoadState("networkidle");
  const ssoButton = page.locator("button:has-text('SSO')");
  await ssoButton.dispatchEvent("click");

  // Should redirect to Open WebUI's OIDC handler, then to Authelia
  await page.waitForURL(
    (url) => url.hostname === "auth.localhost" || url.pathname.includes("/oauth/oidc"),
    { timeout: 15_000 },
  );
  const url = page.url();
  // Either at Authelia or at Open WebUI's OIDC handler (both mean SSO flow started)
  expect(
    url.includes("auth.localhost") || url.includes("/oauth/oidc")
  ).toBe(true);
  await context.close();
});

test("full OIDC login through Authelia creates a session", async ({
  browser,
}) => {
  // Authelia uses HTTPS (self-signed cert), so ignore HTTPS errors.
  const context = await browser.newContext({
    ignoreHTTPSErrors: true,
    viewport: { width: 1280, height: 1600 },
  });
  const page = await context.newPage();

  // 1. Go to Open WebUI — should show login page with SSO button
  await page.goto(OPEN_WEBUI_URL, { timeout: 15_000 });
  await page.waitForLoadState("networkidle");
  const ssoButton = page.locator("button:has-text('SSO')");
  await expect(ssoButton).toBeVisible({ timeout: 15_000 });
  await ssoButton.dispatchEvent("click");

  // 2. Wait for redirect to Authelia (goes through Open WebUI's /oauth/oidc/login first)
  await page.waitForURL(
    (url) => url.hostname === "auth.localhost",
    { timeout: 15_000 },
  );

  // 3. Fill in Authelia credentials
  await loginToAuthelia(page);

  // 4. Should be redirected back to Open WebUI (localhost), now authenticated
  await page.waitForURL(
    (url) => url.hostname === "127.0.0.1" && !url.pathname.startsWith("/oauth"),
    { timeout: 15_000 },
  );

  // 5. Verify we're logged in — page should NOT be the login page anymore
  //    (could be chat, get-started, or admin setup — any authenticated page)
  await page.waitForLoadState("networkidle");
  const body = await page.locator("body").textContent();
  // Login page has "Sign in" / "Sign up" — authenticated pages don't
  expect(body).not.toMatch(/sign in.*sign up/i);

  await context.close();
});
