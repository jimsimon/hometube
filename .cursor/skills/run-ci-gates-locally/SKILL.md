---
name: run-ci-gates-locally
description: Run the full HomeTube CI gate suite locally before pushing - frontend lint/format/typecheck/test plus backend fmt/clippy/test, mirroring .github/workflows/ci.yml. Use before committing or opening a PR to catch failures early.
---

# Run CI gates locally

These commands mirror the jobs in `.github/workflows/ci.yml` and the
`.husky/pre-commit` hook. Run them from the repo root before pushing. Linting is
oxlint/oxfmt + cargo clippy/fmt (there is no ESLint, Prettier, Ruff, or mypy).

## Frontend (lint, format, typecheck, build)

```bash
cd frontend
npm run lint           # oxlint ./src
npm run format:check   # oxfmt --check ./src   (use `npm run format` to fix)
npm run typecheck      # tsc --noEmit
npm test               # vitest run (browser mode, Chromium)
npm run test:coverage  # enforce coverage thresholds (80/80/75/70)
npm run build          # tsc --noEmit && vite build
cd ..
```

If `npm test` cannot launch Chromium, install the browser once:
`cd frontend && npx playwright install --with-deps chromium`.

## Backend (format, lint, test, coverage)

```bash
cargo fmt --all -- --check      # use `cargo fmt --all` to fix
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release --locked
# coverage gate (needs cargo-llvm-cov + llvm-tools-preview):
cargo llvm-cov --fail-under-lines 80
```

## E2E (optional, slower — see the add-playwright-e2e skill)

```bash
cargo build --release --features test-login
./target/release/hometube &
cd e2e && npm install && npx playwright test
```

## One-liner pre-push check

```bash
( cd frontend && npm run lint && npm run format:check && npm run typecheck && npm test ) \
  && cargo fmt --all -- --check \
  && cargo clippy --all-targets --all-features -- -D warnings \
  && cargo test
```

The pre-commit hook only runs staged-file formatting/lint (`lint-staged`) plus
`cargo fmt --check`; the commands above cover the rest of CI that the hook does
not.
