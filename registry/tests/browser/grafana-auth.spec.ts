import { test, expect } from "@playwright/test";

const AUTHELIA_USER = process.env.AUTHELIA_USER || "testuser";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

const GRAFANA_PORT = process.env.RYRA_PORT_HTTP || "3000";
const GRAFANA_URL = `http://127.0.0.1:${GRAFANA_PORT}`;

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

test("full OIDC login through Authelia creates a Grafana session", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const context = await browser.newContext({
    ignoreHTTPSErrors: true,
    viewport: { width: 1280, height: 1600 },
  });
  const page = await context.newPage();

  // 1. Go to Grafana login. The page is a React SPA and renders the
  //    "Sign in with SSO" button below the username/password form.
  //    Name comes from GF_AUTH_GENERIC_OAUTH_NAME=SSO in service.toml.
  await page.goto(`${GRAFANA_URL}/login`, { timeout: 60_000 });
  await page.waitForLoadState("networkidle");

  // 2. Click "Sign in with SSO" — Grafana renders this as an <a> to
  //    /login/generic_oauth when generic_oauth is enabled. Match by
  //    accessible name which covers both <a> and <button> renderings.
  const ssoLink = page.getByRole("link", { name: /sign in with sso/i });
  await expect(ssoLink).toBeVisible({ timeout: 30_000 });
  await ssoLink.click();

  // 3. Redirect to Authelia (via the auth.localhost domain routed by Caddy).
  await page.waitForURL(
    (url) => url.hostname === "auth.localhost",
    { timeout: 15_000 },
  );

  // 4. Fill Authelia credentials.
  await loginToAuthelia(page);

  // 5. Should land back on Grafana authenticated. Wait for any 127.0.0.1
  //    URL that isn't the /login/generic_oauth callback path.
  await page.waitForURL(
    (url) =>
      url.hostname === "127.0.0.1" &&
      !url.pathname.startsWith("/login/generic_oauth"),
    { timeout: 30_000 },
  );

  // 6. Confirm authentication via Grafana's /api/user — returns 200 with
  //    JSON containing the email/login claim from Authelia when signed in.
  const me = await page.request.get(`${GRAFANA_URL}/api/user`);
  expect(me.status()).toBe(200);
  const body = await me.text();
  expect(body).toMatch(/"login":|"email":/);

  await context.close();
});
