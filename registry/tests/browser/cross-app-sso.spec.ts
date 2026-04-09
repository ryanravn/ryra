import { test, expect } from "@playwright/test";

const AUTHELIA_USER = process.env.AUTHELIA_USER || "testuser";
const AUTHELIA_PASSWORD = process.env.AUTHELIA_PASSWORD || "testpassword123";

// Both services accessed through Caddy (HTTPS) so session cookies share the
// same domain scope and OIDC callbacks resolve correctly.
const FORGEJO_URL = "https://git.test.local:8443";
const WHOAMI_URL = "https://whoami.test.local:8443";

test("cross-app SSO: login via forgejo OIDC, then access whoami without re-auth", async ({
  browser,
}) => {
  // Caddy uses a self-signed cert in test environments
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();

  // --- Phase 1: Log in to Forgejo via Authelia OIDC ---

  // 1. Go to forgejo and click SSO
  await page.goto(`${FORGEJO_URL}/user/login`);
  const autheliaLink = page.locator('a[href*="/user/oauth2/Authelia"]');
  await expect(autheliaLink).toBeVisible({ timeout: 15_000 });
  await autheliaLink.click();

  // 2. Land on Authelia login page — wait for React to fully hydrate
  await page.waitForLoadState("networkidle");
  await page.waitForFunction(
    () => {
      const root = document.getElementById("root");
      return root && (Object.keys(root).some(k => k.startsWith("__react")) || (root as any)._reactRootContainer);
    },
    { timeout: 10_000 },
  ).catch(() => {
    // Fallback: if React detection fails, just wait a bit
  });
  const usernameInput = page.locator("#username-textfield");
  await expect(usernameInput).toBeVisible({ timeout: 15_000 });
  await expect(usernameInput).toBeEditable({ timeout: 5_000 });

  await usernameInput.fill(AUTHELIA_USER);
  await page.locator("#password-textfield").fill(AUTHELIA_PASSWORD);
  await page.getByRole("button", { name: /sign in/i }).click();

  // 3. Accept consent if shown (Authelia shows this on first OIDC login)
  try {
    const consent = page.getByRole("button", { name: /accept|consent|allow|approve/i });
    await consent.click({ timeout: 10_000 });
  } catch {
    // No consent screen
  }

  // 4. Should be back on Forgejo, authenticated
  await page.waitForURL(
    (url) => url.hostname === "git.test.local" && !url.pathname.startsWith("/api/oidc"),
    { timeout: 15_000 },
  );
  const userAvatar = page.locator("nav img.ui.avatar").first();
  await expect(userAvatar).toBeVisible({ timeout: 10_000 });

  // --- Phase 2: Access whoami (forward-auth) — should NOT need to log in again ---

  // 5. Navigate to whoami through Caddy — Authelia session cookie should carry
  await page.goto(WHOAMI_URL, {
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

  await context.close();
});
