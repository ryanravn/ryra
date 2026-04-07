import { test, expect } from "@playwright/test";

const IMMICH_PORT = process.env.IMMICH_PORT || "2283";
const IMMICH_URL = `http://127.0.0.1:${IMMICH_PORT}`;

test("immich login page loads", async ({ page }) => {
  await page.goto(IMMICH_URL);
  await page.waitForURL("**/auth/login", { timeout: 15_000 });
  await expect(page).toHaveTitle(/Immich/i);
});

test("immich shows SSO login button when OIDC is configured", async ({
  page,
}) => {
  await page.goto(`${IMMICH_URL}/auth/login`);
  // Immich shows "Login with SSO" button when OAuth is enabled
  const ssoButton = page.locator('button:has-text("SSO")');
  await expect(ssoButton).toBeVisible({ timeout: 15_000 });
});

test("clicking SSO button initiates OIDC flow", async ({ page }) => {
  await page.goto(`${IMMICH_URL}/auth/login`);
  const ssoButton = page.locator('button:has-text("SSO")');
  await ssoButton.click();

  // Wait for navigation away from immich login
  await page.waitForTimeout(3_000);

  // Should have left the login page — OIDC authorization redirect happened
  const url = page.url();
  expect(url).not.toContain("/auth/login");
});
