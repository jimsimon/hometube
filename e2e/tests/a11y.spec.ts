/**
 * Page-level accessibility sweep.
 *
 * Visits each public-facing route and runs the axe-core helper. Routes
 * that require an authenticated child or parent session are skipped
 * until the project ships a session-fixture (see `e2e/fixtures/`).
 *
 * Failures at `critical` or `serious` impact fail CI.
 */

import { test } from '@playwright/test';

import { assertNoCriticalA11yViolations } from '../fixtures/a11y';

interface RouteCase {
  name: string;
  path: string;
  /** Set to true to skip this case until a session fixture exists. */
  needsAuth?: 'parent' | 'child';
}

const ROUTES: RouteCase[] = [
  { name: 'profile picker', path: '/profiles' },
  { name: 'setup wizard', path: '/setup' },
  { name: 'parent home', path: '/parent/home', needsAuth: 'parent' },
  { name: 'parent family', path: '/parent/family', needsAuth: 'parent' },
  { name: 'parent system', path: '/parent/system', needsAuth: 'parent' },
  { name: 'parent activity', path: '/parent/activity', needsAuth: 'parent' },
  { name: 'parent playlists', path: '/parent/playlists', needsAuth: 'parent' },
  { name: 'child home', path: '/child/home', needsAuth: 'child' },
  { name: 'child channels', path: '/child/channels', needsAuth: 'child' },
  { name: 'child playlists', path: '/child/playlists', needsAuth: 'child' },
  { name: 'child bookmarks', path: '/child/bookmarks', needsAuth: 'child' },
];

for (const route of ROUTES) {
  test(`a11y: ${route.name} (${route.path})`, async ({ page }) => {
    test.skip(
      route.needsAuth != null,
      `Requires ${route.needsAuth} session fixture; will be wired up in a follow-up.`,
    );
    await page.goto(route.path);
    await assertNoCriticalA11yViolations(page);
  });
}
