import { test, expect } from "@playwright/test";

const JELLYFIN_PORT = process.env.JELLYFIN_PORT || "8096";
const JELLYFIN_URL = `http://127.0.0.1:${JELLYFIN_PORT}`;
// Domain-based URL through Caddy (HTTPS) — needed for OIDC flows so session
// cookies work with the callback URL.
const JELLYFIN_DOMAIN_URL = "https://jellyfin.test.local:8443";
const AUTHELIA_USER = process.env.AUTHELIA_USER || "testuser";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

/** Fill in Authelia's login form and submit. */
async function loginToAuthelia(page: import("@playwright/test").Page) {
  // Wait for Authelia's React app to fully hydrate before interacting.
  await page.waitForLoadState("networkidle");
  await page
    .waitForFunction(
      () => {
        const root = document.getElementById("root");
        return (
          root &&
          (Object.keys(root).some((k) => k.startsWith("__react")) ||
            (root as any)._reactRootContainer)
        );
      },
      { timeout: 10_000 },
    )
    .catch(() => {
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
    const consent = page.getByRole("button", {
      name: /accept|consent|allow|approve/i,
    });
    await consent.click({ timeout: 10_000 });
  } catch {
    // No consent screen — already authorized or auto-consented
  }
}

test("full OIDC login through Authelia creates a jellyfin session", async ({
  browser,
}) => {
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();

  // 1. Navigate to the SSO start endpoint — this redirects to Authelia
  console.log(`Navigating to: ${JELLYFIN_DOMAIN_URL}/sso/OID/start/authelia`);
  await page.goto(`${JELLYFIN_DOMAIN_URL}/sso/OID/start/authelia`);

  // 2. Wait for navigation — could be Authelia or an error page
  await page.waitForLoadState("domcontentloaded");
  const currentUrl = page.url();
  console.log(`After SSO start, URL is: ${currentUrl}`);
  console.log(`Page title: ${await page.title()}`);

  // 3. Should be on Authelia login now
  await page.waitForURL((url) => url.hostname === "auth.test.local", {
    timeout: 15_000,
  });
  console.log(`On Authelia, URL: ${page.url()}`);

  // 4. Fill in Authelia credentials
  await loginToAuthelia(page);

  // Log where we are after login
  console.log(`After Authelia login, URL: ${page.url()}`);

  // 5. Should be redirected back to Jellyfin after authentication.
  await page.waitForURL(
    (url) => url.hostname === "jellyfin.test.local",
    { timeout: 30_000 },
  );

  // 6. Verify we ended up back on Jellyfin (not stuck on an error page)
  const finalUrl = page.url();
  console.log(`Final URL: ${finalUrl}`);
  expect(finalUrl).toContain("jellyfin.test.local");

  await context.close();
});
