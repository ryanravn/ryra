import { test, expect } from "@playwright/test";

const AUTHELIA_USER = process.env.AUTHELIA_USER || "testuser";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

// With `--auth` + auto-HTTPS promotion, vikunja lives behind Caddy at
// https://vikunja.internal:<caddy_https_port> (default 8443).
const VIKUNJA_URL = process.env.VIKUNJA_URL || "https://vikunja.internal:8443";

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

test("full OIDC login through Authelia creates a session", async ({ browser }) => {
  // Authelia uses HTTPS (self-signed cert), so ignore HTTPS errors.
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

  // Vikunja is a Vue SPA — the button exists in the DOM before the click
  // handler is attached at hydration, so an early click is a no-op and
  // waitForURL then times out. Click, check for navigation, retry up to
  // 8 × 1s so flakes from slow hydration don't fail the test.
  for (let attempt = 0; attempt < 8; attempt++) {
    await ssoButton.first().click();
    try {
      await page.waitForURL(
        (url) => url.hostname === "auth.internal",
        { timeout: 1_500 },
      );
      break;
    } catch {
      if (attempt === 7) throw new Error("SSO click never redirected to auth.internal");
    }
  }

  // 4. Login at Authelia
  await loginToAuthelia(page);

  // 5. Should redirect back to Vikunja (now authenticated)
  await page.waitForURL(
    (url) => url.hostname === "vikunja.internal" && !url.pathname.startsWith("/auth/openid"),
    { timeout: 15_000 },
  );

  // 6. Verify authenticated — not on the login page
  await page.waitForLoadState("networkidle");
  const url = page.url();
  expect(url).not.toContain("/login");

  await context.close();
});
