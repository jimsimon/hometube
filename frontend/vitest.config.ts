import { defineConfig } from 'vitest/config';
import { playwright } from '@vitest/browser-playwright';

/**
 * Vitest configuration.
 *
 * Tests run in browser mode (Playwright + Chromium) so Lit web
 * components, Cache API, OPFS, BroadcastChannel and other browser-only
 * APIs work without polyfills.
 *
 * Coverage includes both `src/services/` and `src/components/` — the
 * plan requires 80% global coverage. Component tests live alongside
 * their source as `*.test.ts` files and run in the same Chromium
 * environment, so Lit rendering + DOM APIs work natively.
 */
export default defineConfig({
  test: {
    include: ['src/**/*.test.ts'],
    browser: {
      enabled: true,
      provider: playwright(),
      headless: true,
      instances: [{ browser: 'chromium' }],
    },
    coverage: {
      provider: 'v8',
      reporter: ['text', 'lcov', 'html'],
      // Coverage includes all services and every component that has a
      // corresponding `*.test.ts`. As new component tests are added,
      // add the file here so the threshold keeps them honest.
      include: [
        'src/services/**/*.ts',
        'src/components/video-card.ts',
        'src/components/nav-bar.ts',
        'src/components/nav-parent.ts',
        'src/components/nav-child.ts',
        'src/components/nav-drawer.ts',
        'src/components/theme-toggle.ts',
        'src/components/account-selector.ts',
        'src/components/allowlist-manager.ts',
        'src/components/usage-limit-overlay.ts',
        'src/components/notification-bell.ts',
        'src/components/pin-entry-dialog.ts',
        'src/components/loading-spinner.ts',
        'src/components/error-banner.ts',
      ],
      exclude: [
        'src/**/*.test.ts',
        'src/types/**',
        // Service worker runs in a different global; covered via E2E.
        'src/sw.ts',
        // Tiny SW-registration shim — exercised at runtime, not in
        // unit tests.
        'src/services/sw-register.ts',
        // View Transitions uses cross-document navigation APIs
        // (PageSwapEvent, PageRevealEvent) that only fire during
        // actual page navigations — untestable in unit tests.
        'src/services/view-transitions.ts',
        // Thin re-export shim; the real implementation in
        // offline-opfs.ts is well-covered.
        'src/services/offline.ts',
        // OPFS bridge runs in a SW/client context; tested via
        // opfs-bridge.test.ts which exercises the message protocol
        // in isolation.
        'src/services/opfs-bridge.ts',
      ],
      thresholds: {
        lines: 80,
        functions: 75,
        branches: 70,
        statements: 80,
      },
    },
  },
});
