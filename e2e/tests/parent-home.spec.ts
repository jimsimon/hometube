/**
 * E2E: Parent home (/parent/home).
 *
 * With a parent session and at least one child account, verifies:
 *   - the allowlist manager component renders
 *   - allowlist tabs appear once a child is selected
 *   - the child-selector dropdown is visible
 */

import { test, expect } from '../fixtures/session';

const BASE = process.env.PLAYWRIGHT_BASE_URL ?? 'http://localhost:3000';

/**
 * Seed a child account without switching the current session.
 * Uses the Playwright request API (separate cookie jar) so the
 * parentPage's session cookie is unaffected.
 */
async function ensureChild(request: import('@playwright/test').APIRequestContext) {
  await request.post(`${BASE}/api/test/seed`, {
    data: { role: 'child', display_name: 'E2E Child' },
  });
}

test('parent home renders allowlist tabs', async ({ parentPage, request }) => {
  await ensureChild(request);
  await parentPage.goto('/parent/home');
  await expect(parentPage).toHaveURL(/\/parent\/home/);

  // The allowlist manager renders tab buttons for channels/videos.
  // Tabs live inside the component's shadow DOM, so use chained locators
  // to pierce the shadow boundary.
  const tabs = parentPage
    .locator('hometube-allowlist-manager')
    .locator('[role="tab"]');
  await expect(tabs.first()).toBeVisible({ timeout: 10_000 });
});

test('parent home shows a child selector', async ({ parentPage, request }) => {
  await ensureChild(request);
  await parentPage.goto('/parent/home');
  // The account-selector component renders a <select> when at least
  // one child exists.
  const select = parentPage.locator('hometube-account-selector');
  await expect(select).toBeVisible({ timeout: 10_000 });
});
