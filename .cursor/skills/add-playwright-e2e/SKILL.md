---
name: add-playwright-e2e
description: Add a Playwright end-to-end test for HomeTube against a server built with the test-login feature. Covers the session fixture (parentPage/childPage), seeding state through /api/test/* endpoints, the a11y sweep with axe-core, and how to build and run the suite locally and in CI. Use when adding E2E coverage for a user flow.
---

# Add a Playwright E2E test

E2E tests live in `e2e/tests/*.spec.ts` and run against a real running server.
The server must be built with the `test-login` cargo feature, which exposes
`/api/test/seed`, `/api/test/login-as`, `/api/test/reset`, and
`/api/test/seed-feed-item`. This feature must NEVER ship to production.

## Steps

1. **Create** `e2e/tests/<flow>.spec.ts`.

2. **Get an authenticated page** via the session fixture
   (`e2e/fixtures/session.ts`):

   ```ts
   import { test, expect } from '../fixtures/session';

   test('child home renders the feed rows', async ({ childPage }) => {
     await childPage.goto('/child/home');
     await expect(
       childPage.getByRole('heading', { name: /continue watching/i }),
     ).toBeVisible({ timeout: 10_000 });
   });
   ```

   `parentPage` and `childPage` already carry a valid `hometube_session`
   cookie (seeded per worker, parallel-safe). For unauthenticated routes, import
   `test` from `@playwright/test` directly.

3. **Seed deterministic state** from inside the page so the cookie applies,
   using the `test-login` endpoints (see `e2e/tests/child-home.spec.ts`):

   ```ts
   await childPage.evaluate(async () => {
     const me = await (await fetch('/api/auth/me', { credentials: 'include' })).json();
     await fetch('/api/test/seed-feed-item', {
       method: 'POST',
       headers: { 'Content-Type': 'application/json' },
       credentials: 'include',
       body: JSON.stringify({ child_account_id: me.id, channel_id: 'UCx', video_id: 'v1', title: 'New upload' }),
     });
   });
   await childPage.goto('/child/home');
   await expect(childPage.getByText('New upload')).toBeVisible({ timeout: 10_000 });
   ```

4. **Prefer role/text locators** (`getByRole`, `getByText`) over CSS so tests
   double as a11y signal. Generous timeouts (~10s) absorb component hydration.

5. **Accessibility:** the `e2e/tests/a11y.spec.ts` sweep runs axe-core via
   `fixtures/a11y` (`assertNoCriticalA11yViolations`) and fails on
   critical/serious violations. Add new public routes to its `ROUTES` array.

## Build and run

```bash
# 1. Build the server WITH test-login and start it (serves :3000)
cargo build --release --features test-login
./target/release/hometube &        # CI uses DATABASE_PATH=./data/database/test.db

# 2. Run the suite
cd e2e && npm install && npx playwright test
# npm run test:headed   # to watch in a browser
```

CI builds frontend + release server with `--features test-login`, starts it,
then runs Playwright from `e2e/`.
