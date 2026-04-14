import { test, expect } from "@playwright/test";

const IMMICH_PORT = process.env.RYRA_PORT_HTTP || process.env.IMMICH_PORT || "2283";
const IMMICH_URL = `http://127.0.0.1:${IMMICH_PORT}`;
const AUTHELIA_USER = process.env.AUTHELIA_USER || "testuser";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

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
    const consent = page.getByRole("button", { name: /accept/i });
    await consent.click({ timeout: 10_000 });
  } catch {
    // No consent screen — already authorized
  }
}

test("full OIDC login through Authelia creates an immich session", async ({
  browser,
}) => {
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();

  // 1. Go to immich login page
  await page.goto(`${IMMICH_URL}/auth/login`, { timeout: 30_000 });
  await page.waitForLoadState("networkidle");

  // 2. Click the SSO button ("Login with SSO")
  const ssoButton = page.locator('button:has-text("SSO")');
  await expect(ssoButton).toBeVisible({ timeout: 15_000 });
  await ssoButton.click();

  // 3. Should redirect to Authelia — fill in credentials
  await page.waitForURL(
    (url) => url.hostname === "auth.localhost",
    { timeout: 15_000 },
  );
  await loginToAuthelia(page);

  // 4. Should be redirected back to immich with the auth code, then to
  //    an authenticated page (onboarding or photos). The callback goes
  //    through /auth/login?code=... before redirecting.
  await page.waitForURL(
    (url) =>
      url.hostname === "127.0.0.1" &&
      !url.searchParams.has("code") &&
      url.pathname !== "/auth/login",
    { timeout: 15_000 },
  );

  await context.close();
});
