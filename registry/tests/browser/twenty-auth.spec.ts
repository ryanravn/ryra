import { test, expect } from "@playwright/test";

const TWENTY_PORT = process.env.RYRA_PORT_HTTP || "3000";
const TWENTY_URL = `http://127.0.0.1:${TWENTY_PORT}`;
const AUTHELIA_USER = process.env.AUTHELIA_USER || "testuser";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

test("twenty welcome page shows SSO button", async ({ page }) => {
  await page.goto(`${TWENTY_URL}/welcome`);
  const ssoButton = page.getByRole("button", { name: /single sign-on|sso/i });
  await expect(ssoButton).toBeVisible({ timeout: 15_000 });
});

test("full OIDC login through Authelia reaches Twenty", async ({ browser }) => {
  // Authelia uses HTTPS with a self-signed cert
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();

  // 1. Go to Twenty welcome and click SSO
  await page.goto(`${TWENTY_URL}/welcome`);
  const ssoButton = page.getByRole("button", { name: /single sign-on|sso/i });
  await expect(ssoButton).toBeVisible({ timeout: 15_000 });
  await ssoButton.click();

  // 2. SSO provider selection — click the first Authelia button
  const autheliaButton = page.getByRole("button", { name: /authelia/i }).first();
  await expect(autheliaButton).toBeVisible({ timeout: 10_000 });
  await autheliaButton.click();

  // 3. Authelia login form — wait for React hydration before interacting
  await page.waitForLoadState("networkidle");
  const usernameInput = page.getByRole("textbox", { name: /username/i });
  await expect(usernameInput).toBeVisible({ timeout: 15_000 });
  await expect(usernameInput).toBeEditable({ timeout: 5_000 });

  await usernameInput.fill(AUTHELIA_USER);
  await page.getByRole("textbox", { name: /password/i }).fill(AUTHELIA_PASSWORD);
  await page.getByRole("button", { name: /sign in/i }).click();

  // 4. Consent screen — click Accept
  const acceptButton = page.getByRole("button", { name: /accept/i });
  await expect(acceptButton).toBeVisible({ timeout: 10_000 });
  await acceptButton.click();

  // 5. Should redirect back to Twenty (callback processed)
  await page.waitForURL(
    (url) => url.hostname === "127.0.0.1",
    { timeout: 15_000 },
  );

  // The SSO user lands on /welcome (new user needs workspace setup)
  // or on the main app if they already have a workspace.
  // Either way, we're back on Twenty — the OIDC flow completed.
  expect(page.url()).toContain("127.0.0.1");

  await context.close();
});
