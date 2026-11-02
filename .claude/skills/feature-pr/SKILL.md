---
name: feature-pr
description: The project's workflow for delivering a feature or fix - one branch per feature, local checks, commit style and a pull request. Use whenever starting new work or preparing changes for review.
---

# Feature branch and pull request workflow

Every feature or fix gets its own branch and ends in a pull request against
`main`. Nothing is committed directly to `main`.

## 1. Start

```sh
git fetch origin main
git checkout -b feature/<short-name> origin/main   # or fix/<short-name>
```

If the design changes, update `docs/architecture.md` in the same branch.

## 2. Implement test-first where possible

- For a bug: first a test that fails, then the fix.
- For a feature: unit tests with the code, integration/equivalence tests for
  anything that touches cutting (see `AGENTS.md`, golden rule).
- `///` docs on every public item; inline comments for non-obvious logic.

## 3. Check locally

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

All four must pass. Never skip, `#[ignore]` or weaken a test to get green.

## 4. Commit

- Small, logically separate commits; imperative subject line under ~70
  characters, a body explaining *why* when it is not obvious.
- Do not commit build output, local fixtures that the generator script does
  not produce, or editor files.

## 5. Pull request

- Push with `git push -u origin <branch>`.
- Fill in `.github/pull_request_template.md`: what changed, why, how it was
  tested, and any limitation (e.g. "sample-exact only on gapless-aware
  players").
- One feature per PR. If the work grew, split it.
- When CI fails, fix the cause in a new commit on the same branch. A test that
  turns red before users see the bug is the process working.
