/**
 * Accessibility helper for Playwright tests.
 *
 * Wraps `@axe-core/playwright`'s AxeBuilder so any test can assert
 * "no critical or serious a11y violations on this page" with one line:
 *
 *   await assertNoCriticalA11yViolations(page);
 *
 * Critical / serious are the bar set by the project's CI policy.
 * Lower-severity violations are surfaced in the report but don't fail
 * the build.
 */

import AxeBuilder from '@axe-core/playwright';
import { expect, type Page } from '@playwright/test';

const FAILING_IMPACTS: ReadonlyArray<string> = ['critical', 'serious'];

export interface A11yOptions {
  /** Restrict the scan to a CSS selector (defaults to the whole page). */
  include?: string;
  /** Tags to enable, e.g. `['wcag2a', 'wcag2aa']`. */
  tags?: string[];
}

/**
 * Run axe-core on `page` and fail the test if any rule violation is
 * tagged `critical` or `serious`.
 */
export async function assertNoCriticalA11yViolations(
  page: Page,
  options: A11yOptions = {},
): Promise<void> {
  let builder = new AxeBuilder({ page });
  if (options.include) builder = builder.include(options.include);
  if (options.tags) builder = builder.withTags(options.tags);

  const result = await builder.analyze();
  const blocking = result.violations.filter(
    (v) => v.impact && FAILING_IMPACTS.includes(v.impact),
  );

  if (blocking.length > 0) {
    const formatted = blocking
      .map(
        (v) =>
          `[${v.impact}] ${v.id}: ${v.help}\n  ${v.helpUrl}\n  Affected nodes:\n` +
          v.nodes
            .slice(0, 5)
            .map((n) => `    - ${n.target.join(', ')}`)
            .join('\n'),
      )
      .join('\n\n');
    expect(blocking, `axe-core found blocking violations:\n${formatted}`).toEqual(
      [],
    );
  }
}
