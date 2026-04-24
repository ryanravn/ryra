import { test, expect } from "@playwright/test";

const AUTHELIA_USER = process.env.AUTHELIA_USER || "testuser";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

// With `--auth` + auto-HTTPS promotion, open-webui lives behind Caddy at
// https://open-webui.internal:<caddy_https_port> (default 8443).
const OPEN_WEBUI_URL = process.env.OPEN_WEBUI_URL || "https://open-webui.internal:8443";

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

// Open WebUI is a heavy SPA — in a VM the JS bundles take a long time to
// load and hydrate, so we need a longer test-level timeout.
test("full OIDC login through Authelia creates a session", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const context = await browser.newContext({
    ignoreHTTPSErrors: true,
    viewport: { width: 1280, height: 1600 },
  });
  const page = await context.newPage();

  // 1. Go to Open WebUI — shows onboarding/login page with SSO button.
  //    Open WebUI is a heavy SPA that can take a long time to hydrate in VMs.
  await page.goto(OPEN_WEBUI_URL, { timeout: 60_000 });
  await page.waitForLoadState("networkidle");

  // 2. Click the SSO button ("Continue with SSO")
  const ssoButton = page.getByRole("button", { name: /continue with sso|sso/i });
  await expect(ssoButton).toBeVisible({ timeout: 60_000 });
  await ssoButton.dispatchEvent("click");

  // 3. Should redirect to Authelia via Open WebUI's /oauth/oidc/login
  await page.waitForURL(
    (url) => url.hostname === "auth.internal",
    { timeout: 15_000 },
  );

  // 4. Fill in Authelia credentials
  await loginToAuthelia(page);

  // 5. Should be redirected back to Open WebUI, now authenticated.
  //    Wait for any URL on the Open WebUI host that isn't the OAuth callback.
  await page.waitForURL(
    (url) => url.hostname === "open-webui.internal" && !url.pathname.startsWith("/oauth"),
    { timeout: 30_000 },
  );

  await context.close();
});
