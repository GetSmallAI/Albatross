# Release checklist

Use this when shipping a new Albatross version. **Updating version fields in
the repo is not a release.** A release exists only after the tag, GitHub
Release workflow, and (when configured) Homebrew tap update.

## Before you start

Confirm with the user:

- **Land on main only** — feature/docs commits, no version bump, no tag.
- **Ship release X.Y.Z** — follow this checklist end-to-end.

Do not bump `Cargo.toml` / `CHANGELOG.md` in a feature commit unless the user
explicitly asked for a release in the same session.

## Pre-push gate (match CI)

Run on the branch before pushing release-related work to `main`:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`cargo test` passing alone is not enough. CI runs clippy with `-D warnings`.

## Release steps

Typical pattern (see `chore: release 1.0.2` on `main`):

1. **Land feature work** on `main` (no version bump).
2. **Release commit** — bump only what the repo uses for releases:
   - `Cargo.toml` / `Cargo.lock` — `version = "X.Y.Z"`
   - `CHANGELOG.md` — move `[Unreleased]` items into `[X.Y.Z] - YYYY-MM-DD`
   - `README.md` — version badge if present
   ```bash
   git commit -m "chore: release X.Y.Z"
   ```
3. **Tag and push** (tags use a leading `v`):
   ```bash
   git tag vX.Y.Z
   git push origin main
   git push origin vX.Y.Z
   ```
4. **Watch** [`.github/workflows/release.yml`](../.github/workflows/release.yml)
   — triggered by `push` of `v*` tags. It builds macOS artifacts, creates the
   GitHub Release, and updates `GetSmallAI/homebrew-tap` when
   `TAP_GITHUB_TOKEN` is set.
5. **Verify release**:
   ```bash
   gh release view vX.Y.Z
   git fetch --tags && git tag -l 'v*' | tail -5
   ```
   Confirm macOS `.tar.gz` assets and `SHA256SUMS` on the release page.
6. **Verify Homebrew tap** (when token configured):
   - `GetSmallAI/homebrew-tap` → `Formula/albatross.rb` shows `version "X.Y.Z"`.
   - Optionally: `brew upgrade albatross` from a machine using the tap.

## If something fails

- **Tag pushed but workflow failed** — fix on `main`, cut a patch release
  (`X.Y.Z+1`) or re-run workflow after fix; do not re-use a broken tag.
- **Version in repo but no tag** — users and Homebrew are still on the old
  release. Complete steps 2–6; do not treat CHANGELOG + Cargo.toml as shipped.

## Scorecard changes (when touching `/scorecard`)

- **Quality PR (`counts`)** — if docs say manual `/scorecard close` can ship a
  quality PR, tests must cover the grid rule, not only the numeric score.
  Manual closes opened outside `gh` should be able to count when local evidence
  is strong (e.g. passing tests + `--url`), unless product docs explicitly say
  otherwise.
- **Acceptance tests** — add or extend tests for `counts`, manual close flags
  (`--url`, `--tests`), and `/scorecard pr <n>` output when behavior changes.
