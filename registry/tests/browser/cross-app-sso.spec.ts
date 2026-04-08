import { test, expect } from "@playwright/test";

const FORGEJO_PORT = process.env.FORGEJO_PORT || "3000";
const FORGEJO_URL = `http://127.0.0.1:${FORGEJO_PORT}`;
const AUTHELIA_USER = process.env.AUTHELIA_USER || "admin";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

// Access whoami through Caddy (HTTPS with forward auth)
const WHOAMI_CADDY_URL = "https://whoami.test.local:8443";

test("cross-app SSO: login via forgejo OIDC, then access whoami without re-auth", async ({
  page,
}) => {
  // --- Phase 1: Log in to Forgejo via Authelia OIDC ---

  // 1. Go to forgejo and click SSO
  await page.goto(`${FORGEJO_URL}/user/login`);
  const autheliaLink = page.locator('a[href*="/user/oauth2/Authelia"]');
  await expect(autheliaLink).toBeVisible({ timeout: 15_000 });
  await autheliaLink.click();

  // 2. Land on Authelia login page — wait for React to hydrate
  const usernameInput = page.locator("#username-textfield");
  await expect(usernameInput).toBeVisible({ timeout: 15_000 });
  await expect(usernameInput).toBeEditable({ timeout: 5_000 });

  await usernameInput.fill(AUTHELIA_USER);
  await page.locator("#password-textfield").fill(AUTHELIA_PASSWORD);
  await page.getByRole("button", { name: /sign in/i }).click();

  // 3. Accept consent if shown
  const consent = page.locator('button:has-text("Accept"), button#accept-btn');
  try {
    await consent.click({ timeout: 5_000 });
  } catch {
    // No consent screen
  }

  // 4. Should be back on Forgejo, authenticated
  await page.waitForURL((url) => url.toString().startsWith(FORGEJO_URL), {
    timeout: 15_000,
  });
  const userMenu = page.locator(
    '.user-menu, [aria-label="Profile and Settings"], img.ui.avatar',
  );
  await expect(userMenu).toBeVisible({ timeout: 10_000 });

  // --- Phase 2: Access whoami (forward-auth) — should NOT need to log in again ---

  // 5. Navigate to whoami through Caddy — Authelia session cookie should carry
  await page.goto(WHOAMI_CADDY_URL, {
    waitUntil: "domcontentloaded",
    timeout: 15_000,
  });

  // 6. Should see whoami's response directly (no Authelia login page)
  const url = page.url();
  const body = await page.locator("body").textContent();

  // Verify we're NOT on the Authelia login page
  expect(new URL(url).hostname).not.toBe("auth.test.local");
  // Verify whoami responded with its echo output
  expect(body).toContain("Hostname:");
});
