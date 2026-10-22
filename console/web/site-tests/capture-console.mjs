// Render the real console with the same synthetic status fixtures used by its browser tests.
import { chromium } from "@playwright/test";
import { readFile, mkdir, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";

const status = JSON.parse(
  await readFile("../../tests/fixtures/status/lending.json", "utf8"),
);
for (const resource of status.resources) {
  resource.required ??= true;
  resource.required_by ??= resource.required
    ? [status.upstreams[0]?.id ?? "configured-route"]
    : [];
  resource.adapter_mapping ??= null;
}
for (const upstream of status.upstreams) {
  upstream.listener ??= {
    state: "unknown",
    quality: "unknown",
    age_s: null,
    last_error: null,
  };
}
const poll = { state: "online", status, console_version: "1.0.0" };
const browser = await chromium.launch();
try {
  const page = await browser.newPage({
    viewport: { width: 1440, height: 760 },
    deviceScaleFactor: 1,
  });
  await page.addInitScript((value) => {
    window.__TAURI__ = { core: { invoke: async () => value } };
  }, poll);
  await page.goto("http://127.0.0.1:38110/");
  await page
    .getByRole("heading", { name: "Room for borrowed work", exact: true })
    .waitFor();
  await page.evaluate(() => document.fonts.ready);
  const directory = "../../website/assets";
  await mkdir(directory, { recursive: true });
  const overview = await page.screenshot({
    path: `${directory}/console-overview.jpg`,
    type: "jpeg",
    quality: 87,
  });
  await page.getByRole("link", { name: "Workloads", exact: true }).click();
  await page.locator("#upstreams article").first().waitFor();
  const workloads = await page.screenshot({
    path: `${directory}/console-workloads.jpg`,
    type: "jpeg",
    quality: 87,
  });
  await writeFile(
    `${directory}/screenshot-provenance.json`,
    JSON.stringify(
      {
        renderer: "Vacancyd owner console frontend",
        version: "1.0.0",
        data: "Synthetic lending fixture; not measurements from a user machine",
        browser: browser.version(),
        viewport: { width: 1440, height: 760 },
        images: [
          {
            name: "console-overview.jpg",
            sha256: createHash("sha256").update(overview).digest("hex"),
          },
          {
            name: "console-workloads.jpg",
            sha256: createHash("sha256").update(workloads).digest("hex"),
          },
        ],
      },
      null,
      2,
    ) + "\n",
  );
  console.log(
    "Captured the real console overview and workload views using synthetic example data.",
  );
} finally {
  await browser.close();
}
