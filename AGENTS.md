# Agent Instructions

## General

- After finishing code or documentation updates, give the user a git commit message.
- Before pushing to `main`, run the CI check suite:
  ```bash
  cargo fmt --all -- --check
  cargo clippy --all-targets -- -D warnings
  cargo test
  ```
  Tests passing alone is not the release bar; clippy with `-D warnings` must pass.

## Releases

**Updating `Cargo.toml`, `Cargo.lock`, `README.md`, and `CHANGELOG.md` is not
shipping a release.** A version is released only after the `vX.Y.Z` tag exists,
the GitHub Release workflow succeeds, and macOS assets are published.

Full step-by-step: **[docs/RELEASE.md](docs/RELEASE.md)**

Summary:

1. Confirm with the user: land on `main` only, or **ship release X.Y.Z**.
2. Prefer separate commits: feature work first, then `chore: release X.Y.Z`.
3. Push `main`, create and push tag `vX.Y.Z`, watch
   [`.github/workflows/release.yml`](.github/workflows/release.yml).
4. Verify: `gh release view vX.Y.Z` (release exists, macOS assets present).
5. Verify Homebrew tap: `GetSmallAI/homebrew-tap` → `Formula/albatross.rb`
   matches the version. Optionally test `brew upgrade albatross`.

Do not mark release work complete until step 4 succeeds.

## Scorecard (when changing `/scorecard` or ship quality scoring)

- Distinguish **numeric score** (0–100) from **`counts`** (increments “quality
  PRs shipped” on the grid). If manual `/scorecard close --url --tests` is
  documented as a quality ship path, add tests that `counts` is true when
  evidence warrants it — not only that the score looks good.
- Manual closes use `opened_by_gh: false`; grid logic must align with `--url` and
  test evidence unless docs explicitly say manual closes are score-only.
