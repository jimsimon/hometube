---
name: write-vitest-browser-test
description: Write a Vitest browser-mode test for a HomeTube Lit component or frontend service. Covers the colocated *.test.ts convention, mounting custom elements and awaiting updateComplete, querying the shadow DOM, asserting events, and the coverage thresholds. Use when adding tests for frontend components or services.
---

# Write a Vitest browser test

Frontend tests run in real Chromium via Vitest browser mode
(`frontend/vitest.config.ts`), so Lit rendering, the DOM, Cache API, OPFS, and
BroadcastChannel all work without polyfills. Tests are colocated with source as
`*.test.ts` and matched by `include: ['src/**/*.test.ts']`.

## Steps

1. **Create** `frontend/src/components/<name>.test.ts` (or
   `frontend/src/services/<name>.test.ts`) next to the file under test.

2. **Follow** `frontend/src/components/error-banner.test.ts`:
   - `import { afterEach, describe, expect, it, vi } from "vitest";`
   - Side-effect import the component to register the custom element:
     `import "./<name>.js";` (note the `.js` suffix), plus
     `import type { <Class> } from "./<name>.js";` for typing.
   - Clean up between tests: `afterEach(() => document.body.querySelectorAll("hometube-<name>").forEach(el => el.remove()));`

3. **Mount + await render:**

   ```ts
   const el = document.createElement("hometube-<name>") as <Class>;
   el.setAttribute("message", "hi");
   document.body.appendChild(el);
   await el.updateComplete;   // wait for Lit's render
   ```

4. **Assert against the shadow root:**

   ```ts
   const banner = el.shadowRoot!.querySelector('[role="alert"]');
   expect(banner!.textContent).toContain("hi");
   ```

5. **Test events** with `vi.fn()` listeners and `.click()`, re-awaiting
   `el.updateComplete` after state-changing interactions.

6. **Mock network** by stubbing the `api` service or `fetch` with `vi`. Many
   components call `api.get("/api/...")` — stub it so tests stay offline and
   deterministic.

## Coverage thresholds

`frontend/vitest.config.ts` enforces lines 80 / statements 80 / functions 75 /
branches 70. New components only count toward coverage once added to the
`coverage.include` list — add your component's path there so the gate keeps it
honest.

## Verify

```bash
cd frontend
npm test
npm run test:coverage   # check thresholds
```
