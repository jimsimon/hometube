/**
 * Session fixture — seeds accounts and mints signed session cookies
 * via the `test-login` feature's HTTP endpoints.
 *
 * Usage in a spec:
 *
 *   import { test } from './fixtures/session';
 *
 *   test('parent home renders', async ({ parentPage }) => {
 *     await parentPage.goto('/parent/home');
 *     ...
 *   });
 *
 * `parentPage` and `childPage` are `Page` objects whose cookie jars
 * already contain a valid `hometube_session` cookie. The underlying
 * accounts are seeded on first use per worker (parallel-safe) via
 * `POST /api/test/seed`.
 */

import { test as base, type Page } from '@playwright/test';

const BASE =
  process.env.PLAYWRIGHT_BASE_URL ?? 'http://localhost:3000';

interface Fixtures {
  parentPage: Page;
  childPage: Page;
}

async function seedAndLogin(
  page: Page,
  role: 'parent' | 'child',
  displayName?: string,
): Promise<void> {
  // The seed endpoint returns a session cookie via Set-Cookie, but
  // Playwright's `request` context doesn't share cookies with pages.
  // Instead we hit the endpoint from *within* the page so the cookie
  // lands in the browser's jar automatically.
  await page.goto(`${BASE}/login`);
  await page.evaluate(
    async ([url, r, n]) => {
      await fetch(url + '/api/test/seed', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ role: r, display_name: n }),
      });
    },
    [BASE, role, displayName ?? `E2E ${role}`] as const,
  );
}

export const test = base.extend<Fixtures>({
  parentPage: async ({ browser }, use) => {
    const ctx = await browser.newContext();
    const page = await ctx.newPage();
    await seedAndLogin(page, 'parent', 'E2E Parent');
    await use(page);
    await ctx.close();
  },
  childPage: async ({ browser }, use) => {
    const ctx = await browser.newContext();
    const page = await ctx.newPage();
    // Seed a parent first (required for allowlist operations).
    await seedAndLogin(page, 'parent', 'E2E Parent (bg)');
    // Then seed and switch to a child.
    await seedAndLogin(page, 'child', 'E2E Child');
    await use(page);
    await ctx.close();
  },
});

export { expect } from '@playwright/test';
