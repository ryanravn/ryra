import { test, expect } from "@playwright/test";

const FORGEJO_PORT = process.env.FORGEJO_PORT || "3000";
const FORGEJO_URL = `http://127.0.0.1:${FORGEJO_PORT}`;
const AUTHELIA_USER = process.env.AUTHELIA_USER || "admin";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

/** Fill in Authelia's login form and submit. */
async function loginToAuthelia(page: import("@playwright/test").Page) {
  const signInBtn = page.getByRole("button", { name: /sign in/i });
  await expect(signInBtn).toBeVisible({ timeout: 15_000 });
  await page.locator("#username-textfield").fill(AUTHELIA_USER);
  await page.locator("#password-textfield").fill(AUTHELIA_PASSWORD);
  await signInBtn.click();

  // Accept consent screen if shown
  const consent = page.locator('button:has-text("Accept"), button#accept-btn');
  try {
    await consent.click({ timeout: 5_000 });
  } catch {
    // No consent screen
  }
}

test("forgejo login page loads", async ({ page }) => {
  await page.goto(FORGEJO_URL);
  await expect(page).toHaveTitle(/Forgejo/, { timeout: 15_000 });
});

test("forgejo has Authelia SSO button", async ({ page }) => {
  await page.goto(`${FORGEJO_URL}/user/login`);
  const autheliaLink = page.locator('a[href*="/user/oauth2/Authelia"]');
  await expect(autheliaLink).toBeVisible({ timeout: 15_000 });
  await expect(autheliaLink).toContainText(/Authelia/);
});

test("clicking SSO button initiates OIDC flow", async ({ page }) => {
  await page.goto(`${FORGEJO_URL}/user/login`);
  const autheliaLink = page.locator('a[href*="/user/oauth2/Authelia"]');
  await autheliaLink.click();

  await page.waitForTimeout(3_000);
  const url = page.url();
  expect(url).not.toContain("/user/login");
});

test("full OIDC login through Authelia creates a forgejo session", async ({
  page,
}) => {
  // 1. Go to forgejo login and click SSO
  await page.goto(`${FORGEJO_URL}/user/login`);
  const autheliaLink = page.locator('a[href*="/user/oauth2/Authelia"]');
  await expect(autheliaLink).toBeVisible({ timeout: 15_000 });
  await autheliaLink.click();

  // 2. Fill in Authelia credentials
  await loginToAuthelia(page);

  // 3. Should be redirected back to forgejo, now authenticated
  await page.waitForURL((url) => url.toString().includes(FORGEJO_URL), {
    timeout: 15_000,
  });

  // 4. Verify we're logged in — forgejo shows the dashboard or user menu
  const userMenu = page.locator(
    '.user-menu, [aria-label="Profile and Settings"], img.ui.avatar',
  );
  await expect(userMenu).toBeVisible({ timeout: 10_000 });
});
