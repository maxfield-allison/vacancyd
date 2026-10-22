import { defineConfig } from '@playwright/test';
export default defineConfig({
  testDir: 'tests', workers: 1,
  use: { baseURL: 'http://127.0.0.1:38110', viewport: { width: 1280, height: 960 } },
  webServer: { command: 'node test-server.mjs', url: 'http://127.0.0.1:38110', reuseExistingServer: false },
});
