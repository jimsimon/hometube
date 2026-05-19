/**
 * Tiny `debounce` helper used by search-style inputs.
 *
 * Wraps a function so that rapid successive calls coalesce into a single
 * trailing-edge invocation, fired `wait` milliseconds after the *last*
 * call. The returned function also exposes:
 *   - `.cancel()` — drop any pending invocation (useful in
 *     `disconnectedCallback` to avoid firing after teardown).
 *   - `.flush()` — invoke immediately with the most recent arguments,
 *     skipping the remaining wait.
 *
 * The wrapper intentionally does NOT return the wrapped function's
 * result; debounced calls are fire-and-forget. If you need the result,
 * have the wrapped function commit it to component state itself.
 *
 * Example:
 *   private runSearch = debounce(() => this.fetchResults(), 300);
 *   // …in disconnectedCallback:
 *   this.runSearch.cancel();
 */
export interface DebouncedFn<Args extends unknown[]> {
  (...args: Args): void;
  cancel(): void;
  flush(): void;
}

export function debounce<Args extends unknown[]>(
  fn: (...args: Args) => unknown,
  wait: number,
): DebouncedFn<Args> {
  let timer: number | null = null;
  let lastArgs: Args | null = null;

  const invoke = (): void => {
    timer = null;
    if (lastArgs) {
      const args = lastArgs;
      lastArgs = null;
      fn(...args);
    }
  };

  const debounced = ((...args: Args): void => {
    lastArgs = args;
    if (timer != null) {
      window.clearTimeout(timer);
    }
    timer = window.setTimeout(invoke, wait);
  }) as DebouncedFn<Args>;

  debounced.cancel = (): void => {
    if (timer != null) {
      window.clearTimeout(timer);
      timer = null;
    }
    lastArgs = null;
  };

  debounced.flush = (): void => {
    if (timer != null) {
      window.clearTimeout(timer);
      invoke();
    }
  };

  return debounced;
}
