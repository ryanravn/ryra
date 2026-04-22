import { test, expect } from "@playwright/test";

const ZAMMAD_PORT = process.env.RYRA_PORT_HTTP || process.env.ZAMMAD_PORT || "8080";
const ZAMMAD_URL = `http://127.0.0.1:${ZAMMAD_PORT}`;
const AUTHELIA_USER = process.env.AUTHELIA_USER || "testuser";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

/** Fill in Authelia's login form and submit, then accept the consent screen. */
async function loginToAuthelia(page: import("@playwright/test").Page) {
  // Wait for Authelia's React app to hydrate before typing — the inputs
  // exist in the server-rendered HTML but only become interactive after
  // hydration wires up the event handlers.
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
    // No consent screen — already authorized.
  }
}

test("full OIDC login through Authelia creates a Zammad session", async ({ browser }) => {
  test.setTimeout(90_000);
  // Authelia runs under a self-signed cert — ignore for tests.
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();

  // 1. Visit Zammad — the SPA routes to #/login. Wait for the login route
  //    to be reached before interacting, otherwise the click can land
  //    before Vue has attached handlers.
  await page.goto(`${ZAMMAD_URL}/`, { timeout: 30_000 });
  await page.waitForURL((url) => url.hash.startsWith("#login"), { timeout: 20_000 });
  await page.waitForLoadState("networkidle");

  // 2. Click the SSO button. Zammad renders it with class
  //    `auth-provider auth-provider--openid-connect` when auth_openid_connect
  //    is enabled. Label comes from `auth_openid_connect_credentials.display_name`.
  //    Use Promise.all with waitForURL so we don't race past the redirect.
  const sso = page.locator("button.auth-provider--openid-connect");
  await expect(sso).toBeVisible({ timeout: 15_000 });
  await Promise.all([
    page.waitForURL((url) => url.hostname === "auth.localhost", { timeout: 30_000 }),
    sso.click(),
  ]);
  await loginToAuthelia(page);

  // 4. Authelia redirects back to Zammad via the callback URL; Zammad
  //    exchanges the code, creates the session, and the SPA navigates to
  //    the dashboard (a hash-route like #clues for admins, #customer for
  //    customers — either is fine, both indicate an authenticated session).
  await page.waitForURL(
    (url) =>
      url.hostname === "127.0.0.1" &&
      !url.searchParams.has("code") &&
      !url.hash.includes("login") &&
      !url.hash.includes("getting_started"),
    { timeout: 30_000 },
  );

  // 5. Verify we're authenticated — "Sign out" only appears in the nav for
  //    logged-in users regardless of role.
  await expect(page.getByText(/sign out/i).first()).toBeVisible({ timeout: 15_000 });

  await context.close();
});
