import type { Page } from "@playwright/test";
import { test, expect } from "./fixtures.js";

test.use({ platform: "macos" });

const next = (page: Page) =>
  page.locator("wizard-shell .footer-right wa-button");

for (const scenario of [
  { failure: "flash-write", flow: "sbc", message: "Write failed: I/O error" },
  {
    failure: "flash-disconnected",
    flow: "sbc",
    message:
      "The storage device was disconnected during the installation. Please reconnect it and try again.",
  },
  {
    failure: "proxmox-install",
    flow: "proxmox",
    message: "Proxmox API error: storage unavailable",
  },
  {
    failure: "utm-create",
    flow: "utm",
    message: "UTM error: Automation permission denied",
  },
] as const) {
  test(`${scenario.failure} shows an error and Try again completes the real wizard flow`, async ({
    page,
  }, testInfo) => {
    test.setTimeout(90000);
    // Set before navigation, but consume once so the same page's retry succeeds.
    await page.addInitScript((failure) => {
      sessionStorage.setItem("hai:mock-failure", failure);
    }, scenario.failure);
    await page.goto("/");
    await page.locator("welcome-view wa-button").click();
    if (scenario.flow === "sbc") {
      await page
        .locator('option-card[title="Raspberry Pi & other boards"]')
        .click();
      await page.locator("device-selection-view device-card").first().click();
      await next(page).click();
      await page.locator("drive-selection-view drive-card").first().click();
      await next(page).click();
      await expect(page.locator("confirmation-view")).toBeVisible();
      await next(page).click();
      await page.locator('confirm-dialog wa-button[variant="danger"]').click();
    } else {
      await page
        .locator(
          `option-card[title="${scenario.flow === "utm" ? "Virtual machine" : "Proxmox server"}"]`
        )
        .click();
      if (scenario.flow === "proxmox") {
        const connect = page.locator("proxmox-connect-view");
        await connect.locator("#server-url").fill("https://192.0.2.10:8006");
        await connect.locator("#username").fill("test@pam");
        await connect.locator("#password").fill("test-only");
      }
      await expect(next(page)).toHaveJSProperty("disabled", false);
      await next(page).click();
      await expect(
        page.locator(`${scenario.flow}-configure-view`)
      ).toBeVisible();
      await expect(next(page)).toHaveJSProperty("disabled", false);
      await next(page).click();
      await expect(page.locator(`${scenario.flow}-confirm-view`)).toBeVisible();
      await next(page).click();
    }
    const prefix = scenario.flow === "sbc" ? "" : `${scenario.flow}-`;
    const progress = page.locator(`${prefix}progress-view`);
    await expect(progress.locator(".error-message")).toHaveText(
      scenario.message,
      { timeout: 30000 }
    );
    await expect(page.locator(`${prefix}success-view`)).toHaveCount(0);
    await expect(next(page)).toHaveText("Try again");
    await expect(
      page.locator("wizard-shell .footer-left wa-button")
    ).toHaveText("Cancel");
    await expect
      .poll(() =>
        page.evaluate(() => sessionStorage.getItem("hai:mock-failure"))
      )
      .toBe(null);
    const screenshot = testInfo.outputPath("failure.png");
    await page.screenshot({ path: screenshot, fullPage: true });
    await testInfo.attach("failure", {
      path: screenshot,
      contentType: "image/png",
    });
    await next(page).click();
    await expect(progress.locator(".error-message")).toHaveCount(0);
    await expect(page.locator("wizard-shell .footer")).not.toBeVisible();
    await expect(page.locator(`${prefix}success-view`)).toBeVisible({
      timeout: 45000,
    });
    await expect(next(page)).toHaveText("Done");
    await next(page).click();
    await expect(page.locator("welcome-view")).toBeVisible();
  });
}
