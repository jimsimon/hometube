/**
 * Shared helpers for component tests.
 *
 * Prefer these over `await new Promise((r) => setTimeout(r, N))` for
 * waiting on promise-driven async work — wall-clock waits are flaky
 * (they pass or fail based on machine load) and slow the suite down.
 */

/**
 * A subset of `LitElement` that's all we need for `flushAsync`. Using
 * a structural type lets tests pass plain `HTMLElement` references
 * (e.g. when the element is created via `insertAdjacentHTML`) without
 * a cast.
 */
interface MaybeLitElement {
  updateComplete?: Promise<boolean>;
}

/**
 * Drain pending microtasks (promise resolutions like a mocked
 * `fetch().then(r => r.json())` chain), then wait for Lit to apply
 * the resulting state to the DOM.
 *
 * Use this after dispatching a form submit, click, or other event
 * that kicks off async work whose effects should be visible by the
 * time the next assertion runs.
 *
 * The microtask drain count is intentionally generous — real-browser
 * `Response.json()` / `Response.text()` involves stream reads that
 * add several internal awaits beyond the user-visible chain.
 *
 * The `updateComplete` loop follows Lit's idiom: when `updateComplete`
 * resolves with `false`, another update was scheduled during the
 * current cycle and we wait for the next one.
 */
export async function flushAsync(el?: MaybeLitElement): Promise<void> {
  // 1. Drain microtasks for in-process promise chains.
  for (let i = 0; i < 20; i++) {
    await Promise.resolve();
  }
  // 2. Yield one macrotask cycle so the event loop can process
  //    platform-scheduled tasks (e.g. real `Response` body stream
  //    reads in browser mode) that are not microtasks.
  //
  //    `MessageChannel.postMessage` is used instead of `setTimeout(_, 0)`
  //    because tests can install `vi.useFakeTimers()` (e.g. the debounce
  //    tests in allowlist-manager.test.ts); fake timers intercept
  //    `setTimeout` but not `MessageChannel`, so this yield works under
  //    both real and fake-timer regimes. It's also not a wall-clock
  //    wait — the yield fires "as soon as currently-queued work is
  //    done", unlike the flaky `setTimeout(_, 10)` pattern this helper
  //    replaces.
  await new Promise<void>((resolve) => {
    const channel = new MessageChannel();
    channel.port1.onmessage = () => {
      channel.port1.close();
      resolve();
    };
    channel.port2.postMessage(null);
  });
  // 3. Final microtask drain to catch promise continuations spawned
  //    during the macrotask above.
  for (let i = 0; i < 5; i++) {
    await Promise.resolve();
  }
  // 4. Wait for Lit to settle. `updateComplete` resolves with `false`
  //    if another update was scheduled during the render; loop until
  //    it returns `true`.
  if (el?.updateComplete) {
    for (let i = 0; i < 5; i++) {
      const settled = await el.updateComplete;
      if (settled) return;
    }
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
