import { test, expect } from "@playwright/test";

const SEAFILE_PORT = process.env.SEAFILE_PORT || "80";
const SEAFILE_URL = `http://127.0.0.1:${SEAFILE_PORT}`;

test("seafile login page loads", async ({ page }) => {
  await page.goto(SEAFILE_URL);
  await expect(page.locator("body")).toContainText(/seafile|log in|sign in/i, {
    timeout: 15_000,
  });
});

test("seafile OAuth endpoint initiates OIDC flow", async ({ page }) => {
  // Seafile doesn't show an OAuth button on the login page by default,
  // but the /oauth/login/ endpoint is available when ENABLE_OAUTH = True.
  // Navigating to it should redirect to the auth provider.
  const response = await page.goto(`${SEAFILE_URL}/oauth/login/`, {
    waitUntil: "domcontentloaded",
    timeout: 15_000,
  });

  // Should redirect (302) to the OAuth authorization URL, not return 404
  const url = page.url();
  expect(url).not.toContain("/accounts/login");
  // Should have been redirected away from seafile (to auth provider)
  // or at least not got a 404
  expect(response?.status()).not.toBe(404);
});
