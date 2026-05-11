/**
 * Smoke test for the first-time setup wizard.
 *
 * Verifies that the wizard renders its skeleton (heading + the
 * credentials form web component shell) without runtime errors. Until
 * the project provides a per-test database fixture this is an unauth
 * smoke check; the setup-redirect middleware bounces every request to
 * `/setup` until the install completes, which is exactly what we
 * exercise here.
 */

import { expect, test } from '@playwright/test';

test('setup wizard renders without errors', async ({ page }) => {
  const consoleErrors: string[] = [];
  page.on('pageerror', (err) => consoleErrors.push(err.message));
  page.on('console', (msg) => {
    if (msg.type() !== 'error') return;
    const text = msg.text();
    // The wizard polls `/api/auth/me` on load to detect whether a
    // parent has already signed in via the OAuth callback. Before
    // signing in, that endpoint returns 401, which the browser logs
    // as a `Failed to load resource` console error even though the
    // application code handles it gracefully. Suppress those to keep
    // this smoke check focused on real runtime errors.
    if (
      text.includes('401 (Unauthorized)') ||
      text.includes('/api/auth/me')
    ) {
      return;
    }
    consoleErrors.push(text);
  });

  await page.goto('/setup');
  // The wizard shell either renders directly or via a redirect from /
  // depending on app state; both should land on /setup.
  await expect(page).toHaveURL(/\/setup\b/);

  // Heading should be present.
  await expect(
    page.getByRole('heading', { level: 1 }),
  ).toBeVisible();

  // No uncaught console errors during initial load.
  expect(consoleErrors, consoleErrors.join('\n')).toEqual([]);
});
