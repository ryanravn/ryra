import { test, expect } from "@playwright/test";

const SYNAPSE_URL = process.env.SYNAPSE_URL || "https://chat.internal:8443";
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

  // Authelia shows a consent screen on first OIDC login for this client.
  try {
    await page.getByRole("button", { name: /accept|consent|allow|approve/i })
      .click({ timeout: 10_000 });
  } catch {
    // Already authorized — no consent screen.
  }
}

test("full OIDC login through Authelia creates a Matrix session", async ({
  browser,
}) => {
  // Caddy uses a self-signed cert in tests.
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();

  // 1. Go to Element's login page.
  await page.goto(`${SYNAPSE_URL}/#/login`);
  const ssoButton = page.getByRole("button", { name: /continue with sso/i });
  await expect(ssoButton).toBeVisible({ timeout: 15_000 });
  await ssoButton.click();

  // 2. Authelia login + consent.
  await page.waitForURL(
    (url) => url.hostname === "auth.internal",
    { timeout: 15_000 },
  );
  await loginToAuthelia(page);

  // 3. After Authelia OK's, Synapse shows a "Continue to your account" page
  //    with a single Continue link before depositing a login token.
  const continueLink = page.getByRole("link", { name: /continue/i });
  await expect(continueLink).toBeVisible({ timeout: 15_000 });
  await continueLink.click();

  // 4. Element should land on its home screen, logged in as testuser.
  await page.waitForURL(
    (url) => url.hostname === "chat.internal" && url.hash.startsWith("#/home"),
    { timeout: 15_000 },
  );
  await expect(page.getByRole("heading", { name: /welcome testuser/i }))
    .toBeVisible({ timeout: 15_000 });

  await context.close();
});
