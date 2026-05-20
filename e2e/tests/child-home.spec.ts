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

test('new videos row populates from the feed cache', async ({ childPage }) => {
  // Inject a feed item directly into `feed_source_items` (bypassing
  // the background RSS poller) so the test is deterministic and
  // doesn't depend on outbound network.
  await childPage.evaluate(async () => {
    // The child session cookie already authenticates `/api/test/*`.
    const meRes = await fetch('/api/auth/me', { credentials: 'include' });
    const me = await meRes.json();
    await fetch('/api/test/seed-feed-item', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      credentials: 'include',
      body: JSON.stringify({
        child_account_id: me.id,
        channel_id: 'UCe2e_test',
        channel_title: 'E2E Channel',
        video_id: 'e2e-vid-1',
        title: 'Brand new upload',
      }),
    });
  });

  // Reload so the row re-fetches.
  await childPage.goto('/child/home');

  // The video card for our seeded item should appear within a few
  // seconds — the handler is now a single DB read.
  await expect(childPage.getByText('Brand new upload')).toBeVisible({
    timeout: 10_000,
  });
});
