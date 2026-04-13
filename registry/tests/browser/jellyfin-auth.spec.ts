import { test, expect } from "@playwright/test";

const JELLYFIN_PORT = process.env.JELLYFIN_PORT || "8096";
const JELLYFIN_URL = `http://127.0.0.1:${JELLYFIN_PORT}`;
const AUTHELIA_USER = process.env.AUTHELIA_USER || "testuser";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

/** Fill in Authelia's login form and submit. */
async function loginToAuthelia(page: import("@playwright/test").Page) {
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
    .catch(() => {});
  const usernameInput = page.locator("#username-textfield");
  await expect(usernameInput).toBeVisible({ timeout: 15_000 });
  await expect(usernameInput).toBeEditable({ timeout: 5_000 });

  await usernameInput.fill(AUTHELIA_USER);
  await page.locator("#password-textfield").fill(AUTHELIA_PASSWORD);
  await page.getByRole("button", { name: /sign in/i }).click();

  try {
    const consent = page.getByRole("button", {
      name: /accept|consent|allow|approve/i,
    });
    await consent.click({ timeout: 10_000 });
  } catch {
    // No consent screen
  }
}

test("SSO login through Authelia", async ({ browser }) => {
  // Authelia uses HTTPS (self-signed cert), so ignore HTTPS errors.
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();

  // Verify the SSO button is configured in branding (rendered by the SPA on the login page)
  const brandingResp = await page.request.get(
    `${JELLYFIN_URL}/Branding/Configuration`,
  );
  const branding = await brandingResp.json();
  expect(branding.LoginDisclaimer).toContain("sso/OID/start/authelia");

  // Start the SSO flow via localhost — this is what clicking the button does
  await page.goto(`${JELLYFIN_URL}/sso/OID/start/authelia`);

  // Should redirect to Authelia login
  await page.waitForURL((url) => url.hostname === "auth.test.local", {
    timeout: 10_000,
  });

  // Fill in Authelia credentials
  await loginToAuthelia(page);

  // Should be redirected back to Jellyfin (localhost)
  await page.waitForURL(
    (url) => url.hostname === "127.0.0.1",
    { timeout: 15_000 },
  );

  expect(page.url()).toContain("127.0.0.1");
  await context.close();
});
