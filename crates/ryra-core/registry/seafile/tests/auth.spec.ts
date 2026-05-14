import { test, expect } from "@playwright/test";

const SEAFILE_URL = `https://seafile.test.local:8443`;

test("seafile login page loads via caddy", async ({ page }) => {
  await page.goto(SEAFILE_URL);
  // Seafile should show a login page
  await expect(page.locator("body")).toContainText(/seafile|log in|sign in/i, {
    timeout: 15_000,
  });
});

test("seafile OAuth endpoint is available", async ({ page }) => {
  // Seafile exposes /oauth/login/ when ENABLE_OAUTH is True in seahub_settings.py
  const response = await page.goto(`${SEAFILE_URL}/oauth/login/`);
  // Should redirect to the OAuth provider (302) or show a page (200), not 404
  const status = response?.status() ?? 0;
  expect([200, 301, 302]).toContain(status);
});

test("authelia login page is accessible", async ({ page }) => {
  await page.goto(`https://auth.test.local:8443`);
  await expect(
    page.locator('input[id="username-textfield"]'),
  ).toBeVisible({ timeout: 15_000 });
  await expect(
    page.locator('input[id="password-textfield"]'),
  ).toBeVisible();
});
