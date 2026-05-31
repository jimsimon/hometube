---
name: add-lit-component
description: Create a new HomeTube frontend Lit web component (hometube-* custom element), wire it into the Vite multi-entry build, reference it from the right Askama page template, and add a colocated Vitest browser test. Use when adding any new UI element to the frontend.
---

# Add a Lit component

HomeTube is a multi-page app: the Rust server renders Askama HTML and each page
loads only the component bundles it needs via `<script type="module">`. Vite
auto-discovers one entry per file in `frontend/src/components/`.

## Steps

1. **Create the component file** at `frontend/src/components/<name>.ts`
   (kebab-case filename). Follow the structure of an existing simple component
   such as `frontend/src/components/theme-toggle.ts`:
   - Import from `lit` and `lit/decorators.js`.
   - Register with `@customElement("hometube-<name>")` — the tag MUST be
     prefixed `hometube-` and kebab-case.
   - Use `static styles = css\`...\`` and reference Web Awesome CSS custom
     properties (e.g. `var(--wa-color-text-quiet)`) rather than hardcoded
     colors so light/dark themes work.
   - Use relative imports with a `.js` suffix (e.g.
     `import { api } from "../services/api.js";`), even though the source is
     `.ts` — this is required by the strict TS + ESM config.
   - Add the `declare global { interface HTMLElementTagNameMap { ... } }` block
     at the bottom so TypeScript knows the tag.
   - Add a top-of-file `/** <hometube-...> ... */` docblock describing behavior.

2. **No Vite config edit needed.** `frontend/vite.config.ts` discovers every
   `*.ts` in `src/components/` automatically and emits
   `dist/components/<name>.js`. (Only `services/*` entries are listed
   explicitly.)

3. **Reference it from the page template** that should render it. Add a script
   tag to the relevant file under `templates/` (e.g. `templates/pages/parent/home.html`,
   `templates/base-child.html`):

   ```html
   <script type="module" src="/assets/components/<name>.js"></script>
   ```

   Then place the element in the page markup: `<hometube-<name>></hometube-<name>>`.
   The `/assets/` prefix is the base path the Rust server mounts `dist/` at.

4. **Add a colocated test** `frontend/src/components/<name>.test.ts` (see the
   `write-vitest-browser-test` skill and `frontend/src/components/error-banner.test.ts`).

5. **If the component should count toward coverage**, add its path to the
   `coverage.include` array in `frontend/vitest.config.ts` (components are
   listed individually there).

## Verify

```bash
cd frontend
npm run typecheck
npm run lint
npm run format:check
npm test
```
