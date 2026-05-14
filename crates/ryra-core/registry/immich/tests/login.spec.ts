import { test, expect } from "@playwright/test";

test("immich login page loads", async ({ page }) => {
  await page.goto("/");
  // Immich redirects to /auth/login
  await page.waitForURL("**/auth/login", { timeout: 15_000 });
  await expect(page).toHaveTitle(/Immich/i);
});

test("login page has email and password fields", async ({ page }) => {
  await page.goto("/auth/login");
  // Use type-based selectors since immich may not use id attributes
  await expect(page.locator('input[type="email"]')).toBeVisible({
    timeout: 10_000,
  });
  await expect(page.locator('input[type="password"]')).toBeVisible();
});

test("can log in with admin credentials", async ({ page }) => {
  const email = process.env.INIT_IMMICH_ADMIN_EMAIL || "admin@example.com";
  const password =
    process.env.INIT_IMMICH_ADMIN_PASSWORD || "testpassword123";

  await page.goto("/auth/login");
  await page.locator('input[type="email"]').fill(email);
  await page.locator('input[type="password"]').fill(password);
  await page.locator('button[type="submit"]').click();

  // After login, should navigate away from /auth/login
  await expect(page).not.toHaveURL(/\/auth\/login/, { timeout: 15_000 });
});
