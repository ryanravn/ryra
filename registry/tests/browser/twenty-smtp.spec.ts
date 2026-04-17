import { test, expect } from "@playwright/test";

const TWENTY_URL = process.env.TWENTY_URL || "https://twenty.localhost:8443";
const INVITE_EMAIL = process.env.INVITE_EMAIL || "smtptest@example.com";

/**
 * Walk Twenty's first-run onboarding (signup → create workspace → profile →
 * skip email sync → invite teammate → finish). The final "Finish" click is
 * what actually enqueues the invitation email — the test is otherwise
 * concerned only with getting through the flow to reach that point.
 */
test("onboarding invite sends an email through the SMTP worker", async ({
  browser,
}) => {
  const context = await browser.newContext({
    ignoreHTTPSErrors: true,
    viewport: { width: 1280, height: 900 },
  });
  const page = await context.newPage();

  // 1. Welcome → Continue with Email
  await page.goto(`${TWENTY_URL}/welcome`);
  await page.getByRole("button", { name: /continue with email/i })
    .click({ timeout: 15_000 });

  // 2. Email → Continue
  await page.getByRole("textbox", { name: /email/i })
    .fill("admin@example.com");
  await page.getByRole("button", { name: /continue/i }).click();

  // 3. Password → Sign up
  await page.getByRole("textbox", { name: /password/i })
    .fill("TestAdmin-12345");
  await page.getByRole("button", { name: /sign up/i }).click();

  // 4. Create workspace
  await page.waitForURL(/\/create\/workspace/, { timeout: 15_000 });
  await page.getByRole("textbox").first().fill("Ryra");
  await page.getByRole("button", { name: /continue/i }).click();

  // 5. Profile → First + last name → Continue
  await page.waitForURL(/\/create\/profile/, { timeout: 15_000 });
  await page.getByRole("textbox", { name: /first name/i }).fill("Ada");
  await page.getByRole("textbox", { name: /last name/i }).fill("L");
  await page.getByRole("button", { name: /continue/i }).click();

  // 6. Skip email sync (the "Continue without sync" link, not the "Continue"
  //    button — we don't want to land on a provider OAuth).
  await page.waitForURL(/\/sync\/emails/, { timeout: 15_000 });
  await page.getByText(/continue without sync/i).click();

  // 7. Invite screen → fill target + Finish
  await page.waitForURL(/\/invite-team/, { timeout: 15_000 });
  await page.getByRole("textbox").first().fill(INVITE_EMAIL);
  await page.getByRole("button", { name: /finish/i }).click();

  // 8. Land inside the workspace — just confirms the mutation didn't 500.
  //    The actual SMTP assertion lives in the test.toml shell step that
  //    polls inbucket's API.
  await page.waitForURL(/\/objects\//, { timeout: 20_000 });

  await context.close();
});
