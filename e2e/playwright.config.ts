/**
 * Playwright configuration for HomeTube end-to-end + accessibility tests.
 *
 * Targets a locally-running app at http://localhost:3000. CI starts the
 * release binary in the background before invoking `playwright test`
 * (see `.github/workflows/ci.yml`); for manual runs, start the app via
 * `tilt up` first.
 */

import { defineConfig, devices } from '@playwright/test';

export default defineConfig({
  testDir: './tests',
  timeout: 30_000,
  fullyParallel: true,
  reporter: [['list'], ['html', { open: 'never', outputFolder: 'report' }]],
  use: {
    baseURL: process.env.PLAYWRIGHT_BASE_URL ?? 'http://localhost:3000',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    video: 'retain-on-failure',
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],
});
