import { test, expect } from "@playwright/test";

// With `--auth` + auto-HTTPS promotion, nextcloud lives behind Caddy at
// https://nextcloud.internal:<caddy_https_port> (default 8443).
// user_oidc refuses to render the SSO button over plain HTTP, so this
// HTTPS hostname is non-negotiable.
const NEXTCLOUD_URL = process.env.NEXTCLOUD_URL || "https://nextcloud.internal:8443";
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

  // First-time consent screen — Authelia prompts on the first OIDC login
  // per client; subsequent logins skip it.
  try {
    const consent = page.getByRole("button", { name: /accept|consent|allow|approve/i });
    await consent.click({ timeout: 10_000 });
  } catch {
    // No consent screen.
  }
}

test("full OIDC login through Authelia creates a Nextcloud session", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  // Caddy serves both nextcloud and authelia with self-signed certs.
  const context = await browser.newContext({
    ignoreHTTPSErrors: true,
    viewport: { width: 1280, height: 1600 },
  });
  const page = await context.newPage();

  // 1. Go to Nextcloud's login page. user_oidc registers itself as an
  //    "alternative login" option that Nextcloud renders alongside the
  //    username/password form once the page hydrates.
  await page.goto(`${NEXTCLOUD_URL}/login`, { timeout: 60_000 });
  await page.waitForLoadState("networkidle");

  // 2. Click the SSO link. user_oidc's LoginController exposes routes at
  //    /apps/user_oidc/login/{providerId} so the href reliably contains
  //    "user_oidc/login". The provider identifier in ryra's auth wiring
  //    is "authelia", which also surfaces in the link's accessible name.
  const ssoLink = page.locator('a[href*="user_oidc/login"]').first();
  await expect(ssoLink).toBeVisible({ timeout: 30_000 });
  await ssoLink.click();

  // 3. Should redirect to Authelia via Caddy.
  await page.waitForURL(
    (url) => url.hostname === "auth.internal",
    { timeout: 15_000 },
  );

  // 4. Fill Authelia credentials + accept consent.
  await loginToAuthelia(page);

  // 5. Should land back on Nextcloud, authenticated. user_oidc sends the
  //    code to /apps/user_oidc/code then Nextcloud redirects to the
  //    default app (usually /apps/dashboard/ or /apps/files/).
  await page.waitForURL(
    (url) =>
      url.hostname === "nextcloud.internal" &&
      !url.pathname.includes("/apps/user_oidc") &&
      !url.pathname.startsWith("/login"),
    { timeout: 30_000 },
  );

  // 6. Verify the user is signed in — Nextcloud 31 renders the avatar menu
  //    inside `<div class="header-end">` with `<div id="user-menu">` for
  //    authenticated sessions (see core/templates/layout.user.php).
  const userMenu = page.locator("#user-menu").first();
  await expect(userMenu).toBeVisible({ timeout: 15_000 });

  await context.close();
});
