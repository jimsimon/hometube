/**
 * E2E: Child home (/child/home).
 *
 * With a child session, verifies:
 *   - the page renders without errors
 *   - the "Continue Watching" + "New Videos" rows appear (empty state OK)
 */

import { test, expect } from '../fixtures/session';

test('child home renders the feed rows', async ({ childPage }) => {
  await childPage.goto('/child/home');
  await expect(childPage).toHaveURL(/\/child\/home/);

  // The two <hometube-video-row> elements render their headings
  // (populated or empty — we're just verifying the shell hydrates).
  await expect(
    childPage.getByRole('heading', { name: /continue watching/i }),
  ).toBeVisible({ timeout: 10_000 });
  await expect(
    childPage.getByRole('heading', { name: /new videos/i }),
  ).toBeVisible({ timeout: 10_000 });
});
