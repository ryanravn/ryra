import { test, expect } from "@playwright/test";

const AUTHELIA_USER = process.env.AUTHELIA_USER || "testuser";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

const OPEN_WEBUI_PORT = process.env.RYRA_PORT_HTTP || process.env.OPEN_WEBUI_PORT || "8080";
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
    // No consent screen — already authorized or auto-consented
  }
}

test("full OIDC login through Authelia creates a session", async ({
  browser,
}) => {
  const context = await browser.newContext({
    ignoreHTTPSErrors: true,
    viewport: { width: 1280, height: 1600 },
  });
  const page = await context.newPage();

  // 1. Go to Open WebUI — shows onboarding/login page with SSO button
  await page.goto(OPEN_WEBUI_URL, { timeout: 30_000 });
  await page.waitForLoadState("networkidle");

  // 2. Click the SSO button ("Continue with SSO")
  //    The button may be below the fold on the onboarding page, so scroll to it first.
  const ssoButton = page.getByRole("button", { name: /continue with sso|sso/i });
  await expect(ssoButton).toBeVisible({ timeout: 30_000 });
  await ssoButton.dispatchEvent("click");

  // 3. Should redirect to Authelia via Open WebUI's /oauth/oidc/login
  await page.waitForURL(
    (url) => url.hostname === "auth.localhost",
    { timeout: 15_000 },
  );

  // 4. Fill in Authelia credentials
  await loginToAuthelia(page);

  // 5. Should be redirected back to Open WebUI, now authenticated.
  //    Wait for any URL on the Open WebUI host that isn't the OAuth callback.
  await page.waitForURL(
    (url) => url.hostname === "127.0.0.1" && !url.pathname.startsWith("/oauth"),
    { timeout: 30_000 },
  );

  await context.close();
});
