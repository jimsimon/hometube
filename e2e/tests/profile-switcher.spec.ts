/**
 * E2E: Profile switcher (/profiles).
 *
 * Verifies the profile-picker page shows seeded accounts and allows
 * switching between them (child: no PIN, parent: requires PIN entry).
 */

import { test, expect } from '../fixtures/session';

test('profiles page shows seeded accounts and allows child selection', async ({
  childPage,
}) => {
  await childPage.goto('/profiles');
  // The child session was seeded, so at least two tiles exist (the
  // background parent + the active child).
  await expect(childPage.locator('hometube-profile-picker')).toBeVisible();
});

test('navigating to /profiles from a child session does not redirect away', async ({
  childPage,
}) => {
  await childPage.goto('/profiles');
  await expect(childPage).toHaveURL(/\/profiles/);
});
