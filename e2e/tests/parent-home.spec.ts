/**
 * E2E: Parent home (/parent/home).
 *
 * With a parent session, verifies:
 *   - the page renders the allowlist tabs
 *   - the child-selector dropdown is visible
 */

import { test, expect } from '../fixtures/session';

test('parent home renders allowlist tabs', async ({ parentPage }) => {
  await parentPage.goto('/parent/home');
  await expect(parentPage).toHaveURL(/\/parent\/home/);

  // The allowlist manager renders tab buttons for channels/playlists/videos.
  const tabs = parentPage.locator('hometube-allowlist-manager [role="tab"]');
  await expect(tabs.first()).toBeVisible({ timeout: 10_000 });
});

test('parent home shows a child selector', async ({ parentPage }) => {
  await parentPage.goto('/parent/home');
  // The account-selector component renders a <select> when at least
  // one child exists.
  const select = parentPage.locator('hometube-account-selector');
  await expect(select).toBeVisible({ timeout: 10_000 });
});
