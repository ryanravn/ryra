import { test, expect } from "@playwright/test";

const AUTHELIA_USER = process.env.AUTHELIA_USER || "testuser";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

const VIKUNJA_URL = "https://tasks.test.local:8443";

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
    // No consent screen
  }
}

test("vikunja login page loads and has SSO option", async ({ browser }) => {
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();
  await page.goto(VIKUNJA_URL, { timeout: 15_000 });
  await page.waitForLoadState("networkidle");

  // Vikunja shows a login page with an SSO button
  const ssoButton = page.locator("a:has-text('SSO'), button:has-text('SSO')");
  await expect(ssoButton).toBeVisible({ timeout: 15_000 });
  await context.close();
});

test("full OIDC login through Authelia creates a session", async ({ browser }) => {
  const context = await browser.newContext({
    ignoreHTTPSErrors: true,
    viewport: { width: 1280, height: 1600 },
  });
  const page = await context.newPage();

  // 1. Go to Vikunja login page
  await page.goto(VIKUNJA_URL, { timeout: 15_000 });
  await page.waitForLoadState("networkidle");

  // 2. Click SSO button
  const ssoButton = page.locator("a:has-text('SSO'), button:has-text('SSO')");
  await expect(ssoButton).toBeVisible({ timeout: 15_000 });
  await ssoButton.first().click();

  // 3. Should redirect to Authelia
  await page.waitForURL(
    (url) => url.hostname === "auth.test.local",
    { timeout: 15_000 },
  );

  // 4. Login at Authelia
  await loginToAuthelia(page);

  // 5. Should redirect back to Vikunja, now authenticated
  await page.waitForURL(
    (url) => url.hostname === "tasks.test.local" && !url.pathname.startsWith("/auth/openid"),
    { timeout: 15_000 },
  );

  // 6. Verify authenticated — not on the login page
  await page.waitForLoadState("networkidle");
  const url = page.url();
  expect(url).not.toContain("/login");

  await context.close();
});
