import { test, expect } from "@playwright/test";

const SEAFILE_DOMAIN = process.env.SEAFILE_DOMAIN || "seafile.localhost:8443";
const SEAFILE_URL = `https://${SEAFILE_DOMAIN}`;
const AUTHELIA_USER = process.env.AUTHELIA_USER || "admin";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

/** Fill in Authelia's login form and submit. */
async function loginToAuthelia(page: import("@playwright/test").Page) {
  // Wait for Authelia's React app to fully hydrate before interacting.
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

  // Accept consent screen if shown (Authelia shows this on first OIDC login)
  try {
    const consent = page.getByRole("button", { name: /accept/i });
    await consent.click({ timeout: 10_000 });
  } catch {
    // No consent screen — already authorized or auto-consented
  }
}

test("full OIDC login through Authelia creates a seafile session", async ({
  browser,
}) => {
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();

  // 1. Go to seafile login page
  await page.goto(`${SEAFILE_URL}/accounts/login/`);
  await expect(page.getByRole("heading", { name: "Log In" })).toBeVisible({
    timeout: 15_000,
  });

  // 2. Click the "Single Sign-On" button to initiate OIDC flow
  const ssoButton = page.getByRole("button", { name: /single sign-on/i });
  await expect(ssoButton).toBeVisible();
  await ssoButton.click();

  // 3. Should redirect to Authelia — fill in credentials
  await loginToAuthelia(page);

  // 4. Should be redirected back to seafile, now authenticated
  await page.waitForURL(
    (url) => url.hostname === SEAFILE_DOMAIN.split(":")[0],
    { timeout: 15_000 },
  );

  // 5. Verify we're logged in — seafile shows the Files page with sidebar nav
  await expect(page.getByRole("heading", { name: "Files" })).toBeVisible({
    timeout: 10_000,
  });

  await context.close();
});
