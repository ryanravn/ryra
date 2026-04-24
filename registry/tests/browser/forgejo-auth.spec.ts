import { test, expect } from "@playwright/test";

// With `--auth` + auto-HTTPS promotion, ryra exposes the service behind
// Caddy at https://<svc>.internal:<caddy_https_port> (default 8443).
// Direct `http://127.0.0.1:<port>` still works for liveness checks but
// OIDC flows must go through the HTTPS hostname so redirect URIs match.
const FORGEJO_URL = process.env.FORGEJO_URL || "https://forgejo.internal:8443";
const AUTHELIA_USER = process.env.AUTHELIA_USER || "testuser";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

/** Fill in Authelia's login form and submit. */
async function loginToAuthelia(page: import("@playwright/test").Page) {
  // Wait for Authelia's React app to fully hydrate before interacting.
  // The HTML renders server-side but form submission requires React event handlers.
  // Wait for the root element to have React's internal properties attached.
  await page.waitForLoadState("networkidle");
  await page.waitForFunction(
    () => {
      const root = document.getElementById("root");
      // React 18+ attaches __reactFiber or _reactRootContainer to the root element
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
    const consent = page.getByRole("button", { name: /accept|consent|allow|approve/i });
    await consent.click({ timeout: 10_000 });
  } catch {
    // No consent screen — already authorized or auto-consented
  }
}

test("clicking SSO button initiates OIDC flow", async ({ browser }) => {
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();
  await page.goto(`${FORGEJO_URL}/user/login`);
  const autheliaLink = page.locator('a[href*="/user/oauth2/Authelia"]');
  await autheliaLink.click();

  await page.waitForTimeout(3_000);
  const url = page.url();
  expect(url).not.toContain("/user/login");
  await context.close();
});

test("full OIDC login through Authelia creates a forgejo session", async ({
  browser,
}) => {
  // Both forgejo and authelia are behind Caddy with self-signed certs.
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();

  // 1. Go to forgejo login and click SSO
  await page.goto(`${FORGEJO_URL}/user/login`);
  const autheliaLink = page.locator('a[href*="/user/oauth2/Authelia"]');
  await expect(autheliaLink).toBeVisible({ timeout: 15_000 });
  await autheliaLink.click();

  // 2. Fill in Authelia credentials
  await loginToAuthelia(page);

  // 3. Should be redirected back to forgejo, now authenticated
  await page.waitForURL(
    (url) =>
      url.hostname === "forgejo.internal" &&
      !url.pathname.startsWith("/api/oidc") &&
      !url.pathname.startsWith("/user/oauth2"),
    { timeout: 15_000 },
  );

  // 4. Verify we're logged in — forgejo shows the user avatar in the navbar
  const userAvatar = page.locator('nav img.ui.avatar').first();
  await expect(userAvatar).toBeVisible({ timeout: 10_000 });

  await context.close();
});
