import { defineConfig } from "@playwright/test";
export default defineConfig({
  testDir: "site-tests",
  testMatch: "*.spec.mjs",
  workers: 1,
  use: {
    baseURL: "http://127.0.0.1:38111",
    reducedMotion: "reduce",
    trace: "retain-on-failure",
    launchOptions: process.env.CHROMIUM_PATH
      ? {
          executablePath: process.env.CHROMIUM_PATH,
          args: ["--use-angle=swiftshader", "--enable-unsafe-swiftshader"],
        }
      : undefined,
  },
  webServer: [
    {
      command:
        "python3 -m http.server 38111 --bind 127.0.0.1 --directory ../../build/website",
      url: "http://127.0.0.1:38111",
      reuseExistingServer: !process.env.CI,
    },
    {
      command:
        "python3 -m http.server 38112 --bind 127.0.0.1 --directory ../../site",
      url: "http://127.0.0.1:38112",
      reuseExistingServer: !process.env.CI,
    },
  ],
});
