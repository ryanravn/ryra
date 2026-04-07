import { test, expect } from "@playwright/test";

const FORGEJO_URL = `https://git.test.local:8443`;

test("forgejo login page loads via caddy", async ({ page }) => {
  await page.goto(FORGEJO_URL);
  await expect(page).toHaveTitle(/Forgejo/, { timeout: 15_000 });
});

test("forgejo has OIDC login option configured", async ({ page }) => {
  await page.goto(`${FORGEJO_URL}/user/login`);
  // Forgejo renders OAuth providers in #oauth2-login-navigator
  const oauthSection = page.locator("#oauth2-login-navigator");
  await expect(oauthSection).toBeVisible({ timeout: 15_000 });
  // Should have at least one OAuth login link (Authelia provider)
  const oauthLinks = oauthSection.locator("a");
  await expect(oauthLinks.first()).toBeVisible();
});

test("authelia login page is accessible", async ({ page }) => {
  // Verify authelia's login page loads through caddy
  await page.goto(`https://auth.test.local:8443`);
  // Authelia should show a login form
  await expect(
    page.locator('input[id="username-textfield"]'),
  ).toBeVisible({ timeout: 15_000 });
  await expect(
    page.locator('input[id="password-textfield"]'),
  ).toBeVisible();
});
