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
  // Use the domain URL (through Caddy) so the OIDC redirect_uri matches.
  // The SSO plugin doesn't add a button to the login page — navigate directly
  // to the SSO start endpoint which begins the OIDC authorization flow.
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();

  // 1. Navigate to the SSO start endpoint — this redirects to Authelia
  await page.goto(`${JELLYFIN_DOMAIN_URL}/sso/OID/start/authelia`);

  // 2. Should redirect to Authelia login
  await page.waitForURL((url) => url.hostname === "auth.test.local", {
    timeout: 15_000,
  });

  // 3. Fill in Authelia credentials
  await loginToAuthelia(page);

  // 4. Should be redirected back to Jellyfin after authentication.
  // The SSO plugin callback returns HTML that completes the login client-side.
  await page.waitForURL(
    (url) => url.hostname === "jellyfin.test.local",
    { timeout: 15_000 },
  );

  // 5. Verify we ended up back on Jellyfin (not stuck on an error page)
  const finalUrl = page.url();
  expect(finalUrl).toContain("jellyfin.test.local");

  await context.close();
});
