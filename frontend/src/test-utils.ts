/**
 * Shared helpers for component tests.
 *
 * Prefer these over `await new Promise((r) => setTimeout(r, N))` for
 * waiting on promise-driven async work — wall-clock waits are flaky
 * (they pass or fail based on machine load) and slow the suite down.
 */

import type { LitElement } from "lit";

/**
 * Drain pending microtasks (promise resolutions like a mocked
 * `fetch().then(r => r.json())` chain), then wait for Lit to apply
 * the resulting state to the DOM.
 *
 * Use this after dispatching a form submit, click, or other event
 * that kicks off async work whose effects should be visible by the
 * time the next assertion runs.
 *
 * The microtask drain count is deliberately generous — four turns
 * covers the typical `fetch → response → json/text → setter` chain
 * with headroom for one extra `await` step in handlers.
 */
export async function flushAsync(el?: LitElement): Promise<void> {
  for (let i = 0; i < 4; i++) {
    await Promise.resolve();
  }
  if (el) {
    await el.updateComplete;
  }
}

/**
 * Poll a predicate until it returns `true`, draining microtasks
 * between attempts. Throws after `attempts` failed polls.
 *
 * Use this when the number of microtask turns required to reach a
 * state is unknown or could grow as the implementation changes (e.g.
 * an async chain wrapped in retry logic). Unlike `flushAsync`, this
 * is robust to that growth.
 */
export async function waitFor(
  predicate: () => boolean,
  { attempts = 50, label = "condition" }: { attempts?: number; label?: string } = {},
): Promise<void> {
  for (let i = 0; i < attempts; i++) {
    if (predicate()) return;
    await Promise.resolve();
  }
  throw new Error(`waitFor: ${label} never became true after ${attempts} microtask turns`);
}
