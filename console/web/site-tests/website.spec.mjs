import { test, expect } from "@playwright/test";

for (const width of [1440, 390, 320]) {
  test(`product page and lending illustration at ${width}px`, async ({
    page,
  }) => {
    await page.setViewportSize({ width, height: 1000 });
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto("/");
    await expect(page.getByRole("heading", { level: 1 })).toHaveText(
      "Spare compute.On your terms.",
    );
    await expect(page.locator("#release-status")).toContainText(
      "Signed downloads are available",
    );
    await expect(
      page.getByRole("link", { name: "releases page", exact: true }),
    ).toHaveAttribute(
      "href",
      "https://github.com/maxfield-allison/vacancyd/releases/latest",
    );
    await expect(page.locator("#release-downloads")).toBeHidden();
    await expect(page.locator('link[rel="canonical"]')).toHaveAttribute(
      "href",
      "https://vacancyd.com/",
    );
    await expect(page.locator("script[data-website-id]")).toHaveCount(0);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
    await page.getByRole("button", { name: "Owner at the desk" }).click();
    await expect(page.locator(".vacancy-sign")).toHaveAccessibleName(
      "No vacancy",
    );
    await expect(
      page.getByRole("button", { name: "Owner at the desk" }),
    ).toHaveAttribute("aria-pressed", "true");
    await expect(page.locator("#route-result")).toHaveText(
      "New request declined",
    );
    await page.getByRole("button", { name: "Machine unattended" }).click();
    await expect(page.locator(".vacancy-sign")).toHaveAccessibleName("Vacancy");
    expect(
      await page
        .locator("img")
        .evaluateAll((images) =>
          images.every((image) => image.complete && image.naturalWidth > 0),
        ),
    ).toBe(true);
    await page.screenshot({
      path: `test-results/website-${width}.png`,
      fullPage: true,
    });
    expect(errors).toEqual([]);
  });
}

test("keyboard opens the privacy page", async ({ page }) => {
  await page.goto("/");
  await page.keyboard.press("Tab");
  await expect(
    page.getByRole("link", { name: "Skip to content" }),
  ).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(page.locator("main")).toBeInViewport();
  const privacy = page.getByRole("link", { name: "Privacy", exact: true });
  await privacy.focus();
  await page.keyboard.press("Enter");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(
    "Website privacy.",
  );
});

test("all core product content remains available without JavaScript", async ({
  browser,
}) => {
  const context = await browser.newContext({ javaScriptEnabled: false });
  try {
    const page = await context.newPage();
    await page.goto("http://127.0.0.1:38111/");
    await expect(
      page.getByRole("heading", {
        name: /Your hardware\.\s*Your house rules\./,
      }),
    ).toBeVisible();
    await expect(page.getByRole("link", { name: "View source" })).toBeVisible();
    await expect(page.locator("#release-status")).toContainText(
      "Signed downloads are available",
    );
    await expect(
      page.getByRole("link", { name: "releases page", exact: true }),
    ).toHaveAttribute(
      "href",
      "https://github.com/maxfield-allison/vacancyd/releases/latest",
    );
  } finally {
    await context.close();
  }
});

test("verified release links appear; foreign destinations remain unavailable", async ({
  page,
}) => {
  await page.route("**/release.json", (route) =>
    route.fulfill({
      json: {
        available: true,
        version: "1.0.0",
        downloads: [
          {
            url: "https://github.com/maxfield-allison/vacancyd/releases/download/v1.0.0/vacancyd.msi",
            label: "Download for Windows",
            event: "download-windows",
          },
        ],
      },
    }),
  );
  await page.goto("/");
  await expect(
    page.getByRole("link", { name: "Download for Windows" }),
  ).toHaveAttribute("data-umami-event", "download-windows");
  await expect(page.locator("#release-status")).toContainText(
    "1.0.0 is available",
  );
  await page.route("**/release.json", (route) =>
    route.fulfill({
      json: {
        available: true,
        version: "1.0.0",
        downloads: [
          {
            url: "https://example.invalid/foreign.msi",
            label: "Untrusted download",
          },
        ],
      },
    }),
  );
  await page.reload();
  await expect(
    page.getByRole("link", { name: "Untrusted download" }),
  ).toHaveCount(0);
  await expect(page.locator("#release-downloads")).toBeHidden();
});

for (const width of [1440, 390]) {
  test(`docs navigation and real search at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 1000 });
    await page.goto("http://127.0.0.1:38112/");
    await expect(page.locator("h1")).toContainText("vacancyd");
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    await page.screenshot({ path: `test-results/docs-${width}.png`, fullPage: true });
    const trigger = page.getByRole("button", { name: /Search/ });
    await trigger.click();
    if (width === 390) {
      await expect(page.getByRole("combobox", { name: "Search" })).toBeVisible();
      await page.keyboard.press("Escape");
      await trigger.focus();
      await page.keyboard.press("Enter");
    }
    await page.getByPlaceholder("Search", { exact: true }).fill("admission");
    const result = page.getByRole("option", {
      name: "Owner return",
      exact: true,
    });
    await expect(result).toBeVisible();
    await result.click();
    await expect(page).toHaveURL(/\/admission\/(?:\?[^#]*)?#owner-return$/);
    await expect(page.locator("h1")).toContainText(
      "Borrowed work and resource ownership",
    );
    await expect(page.locator('meta[name="description"]')).toHaveAttribute(
      "content",
      /cancel, drain and finish/,
    );
  });
}
