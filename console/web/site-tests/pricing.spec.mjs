import { test, expect } from "@playwright/test";

test.setTimeout(150000);

async function ready(page) {
  await page.goto("/pricing/");
  await expect(page.locator(".stage.is-live")).toBeVisible({ timeout: 120000 });
}

for (const width of [1280, 390, 320]) {
  test(`pricing scene and real actions at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 1000 });
    const errors = [];
    const failures = [];
    page.on("pageerror", error => errors.push(error.message));
    page.on("response", response => { if (response.status() >= 400) failures.push(response.status() + " " + response.url()); });
    await ready(page);
    await expect(page).toHaveTitle("Vacancyd · Pricing");
    await expect(page.locator('link[rel="canonical"]')).toHaveAttribute("href", "https://vacancyd.com/pricing/");
    await expect(page.locator("[data-preview], .preview-ribbon, dialog")).toHaveCount(0);
    await expect(page.locator(".plan .price")).toHaveCount(3);
    expect(await page.locator(".plan .price").allTextContents()).toEqual(["$0 per night", "$0 per night", "$0 per night"]);
    await expect(page.getByRole("link", { name: "Get vacancyd", exact: true })).toHaveAttribute("href", "/#get-vacancyd");
    await expect(page.getByRole("link", { name: "Read the docs", exact: true })).toHaveAttribute("href", "https://docs.vacancyd.com/");
    await expect(page.getByRole("link", { name: "Sponsor on GitHub", exact: true })).toHaveAttribute("href", /github.com\/sponsors\/maxfield-allison\?/);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    expect(await page.evaluate(() => window.rivetStage.state().mode)).toBe("still");
    await page.locator("[data-stage]").screenshot({ path: `test-results/pricing-${width}.png` });
    expect(errors).toEqual([]);
    expect(failures).toEqual([]);
    await page.goto("/");
    await expect(page.locator('.nav-links a[href="/pricing/"]')).toBeVisible();
    await page.locator('.nav-links a[href="/pricing/"]').click();
    await expect(page).toHaveURL(/\/pricing\/$/);
  });
}

test("requested playback pauses, resumes and skips to free", async ({ page }) => {
  await ready(page);
  await page.locator('[data-action="replay"]').click();
  await page.waitForFunction(() => window.rivetStage.state().t > 0.15);
  await page.locator('[data-action="pause"]').click();
  const paused = await page.evaluate(() => window.rivetStage.state().t);
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  expect(await page.evaluate(() => window.rivetStage.state().t)).toBe(paused);
  await page.locator('[data-action="pause"]').click();
  await page.waitForFunction(t => window.rivetStage.state().t > t, paused);
  await page.locator('[data-action="skip"]').click();
  expect(await page.evaluate(() => window.rivetStage.state().mode)).toBe("still");
  await expect(page.locator('[data-action="skip"]')).toBeDisabled();
});

test("the full scene plays once into idle", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "no-preference" });
  await ready(page);
  await page.waitForFunction(() => window.rivetStage.state().mode === "idle", null, { timeout: 90000 });
  const state = await page.evaluate(() => window.rivetStage.state());
  expect(state.t).toBe(7);
  expect(state.running).toBe(true);
});

test("missing artwork keeps the finished poster and free offer", async ({ page }) => {
  await page.route("**/pricing/art/manifest.json", route => route.fulfill({ status: 503, body: "Unavailable" }));
  await page.goto("/pricing/");
  await expect(page.locator('[data-stage]')).toHaveAttribute("data-failed", "true", { timeout: 120000 });
  await expect(page.locator(".stage__poster img")).toBeVisible();
  await expect(page.locator('[data-action="replay"]')).toBeDisabled();
  await expect(page.getByRole("heading", { name: "Every room is free." })).toBeVisible();
  await expect(page.getByRole("link", { name: "Get vacancyd", exact: true })).toHaveAttribute("href", "/#get-vacancyd");
});

test("without JavaScript the poster, free prices and live links remain", async ({ browser }) => {
  const context = await browser.newContext({ javaScriptEnabled: false, viewport: { width: 390, height: 844 } });
  try {
    const page = await context.newPage();
    await page.goto("http://127.0.0.1:38111/pricing/");
    await expect(page.locator(".stage__poster img")).toBeVisible();
    await expect(page.getByRole("heading", { name: "Every room is free." })).toBeVisible();
    await expect(page.getByRole("link", { name: "Sponsor on GitHub" })).toHaveAttribute("href", /github.com\/sponsors\/maxfield-allison/);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  } finally {
    await context.close();
  }
});
