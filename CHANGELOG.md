# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Added durable `.albatross/routes.jsonl` routing receipts plus `/route explain`,
  `/route history`, and `/route spend` for inspecting candidate snapshots,
  selector rationale, requested and provider-resolved models, requested and
  effective effort, tokens, latency, cost source, and aggregate spend.

### Security

- Project-local MCP server commands now require per-workspace, configuration-hash
  trust before Albatross spawns them. MCP child processes start with a cleared,
  allowlisted environment plus only explicitly configured variables, and MCP
  requests have a bounded timeout.
- Workspace path enforcement now resolves existing and dangling symlinks before
  allowing file operations, batch edits, checkpoints, or session-path restores,
  preventing symlink traversal outside a denied workspace.
- Shell timeout and cancellation now terminate the full process group and bound
  output-pipe cleanup so background descendants cannot outlive or indefinitely
  stall a tool call.

### Changed

- Replaced the legacy Small Harness terminal wordmark with an Albatross
  wordmark and corrected remaining present-tense README references.
- Selector and routed-planner calls now contribute to session cost accounting,
  and routed coding/plan-execution turns are correlated back to their route ID.
- The supported Rust minimum is now declared and continuously tested as 1.86,
  matching the locked dependency graph.

## [2.0.2] - 2026-07-25

### Changed

- **Moved off the GetSmallAI org onto personal ownership.** The source repo is
  now [morganlinton/Albatross](https://github.com/morganlinton/Albatross), and
  Homebrew installs with `brew install morganlinton/tap/albatross`. Old
  `GetSmallAI/*` GitHub URLs redirect. If you previously tapped
  `getsmallai/tap`, retap:

  ```
  brew untap getsmallai/tap
  brew install morganlinton/tap/albatross
  ```

## [2.0.1] - 2026-07-25

### Fixed

- **The 2.0.0 rename migration could silently strand credentials and notes.** It
  skipped entirely when the new directory already existed, and the new directory
  was easy to create by accident: `client_version()` persisted its cache on every
  call, including from a test, and any single run creates `<workspace>/.albatross/`.
  A user in that state stayed logged out after upgrading, with `auth.json` sitting
  in `~/.config/small-harness`, and a hand-written `rubric.md` stayed in
  `.small-harness/`. Migration now carries over the entries the new location
  lacks instead of skipping, still never overwriting a file that is already
  there. If you upgraded to 2.0.0 and lost your logins, they return on first run
  of 2.0.1.
- `client_version()` no longer writes, so reading the Grok client version cannot
  create the config directory as a side effect. The cache is written on login, as
  intended. This also stops `cargo test` from writing to the real
  `~/.config/albatross`.

## [2.0.0] - 2026-07-25

### Changed

- **Renamed from Small Harness to Albatross.** The binary is now `albatross`,
  installed with `brew install getsmallai/tap/albatross`. The config directory
  moves from `~/.config/small-harness` to `~/.config/albatross`, and per-project
  scratch from `<workspace>/.small-harness/` to `<workspace>/.albatross/`.
  Hook environment variables are now `ALBATROSS_*`.

  Migration is automatic. An existing `~/.config/small-harness` directory is
  moved on first run, so stored OAuth logins, API keys, and scorecard history
  carry over with no re-login, and a `<workspace>/.small-harness/` directory is
  moved when that project is next opened, preserving a hand-written `rubric.md`
  or `spec.md`. Neither move overwrites an existing Albatross directory. Hook
  scripts reading the old `SMALL_HARNESS_*` variables keep working — both
  prefixes are exported for this release, and the aliases will be removed in a
  later one.

  One thing to do by hand: reinstall rather than upgrade if you installed the
  old Homebrew formula, since Homebrew does not follow a formula rename. The
  Codex `originator` request header intentionally keeps its previous value.

## [1.2.7] - 2026-07-21

### Changed

- **Stable prompt-cache prefixes and cache-hit reporting.** Interactive turns
  keep the system prompt and static project instructions byte-stable while
  injecting the prompt-focused repo map only into the current request. OpenAI
  requests use a stable cache-routing key, compatible providers retain their
  native caching behavior, and provider-reported cached input tokens appear in
  the turn footer. Request-only project context is included in context-budget
  and compaction decisions without leaking into persisted session history.

## [1.2.6] - 2026-07-21

### Added

- **`grok` backend — SuperGrok / X Premium+ OAuth.** New cloud backend that
  authenticates with xAI subscription OAuth (Authorization Code + PKCE browser
  login, plus RFC 8628 device-code for headless/SSH) via `/login grok`. Access
  and refresh tokens are stored in the existing `0600` `auth.json` under the
  `grok` provider, refreshed automatically, and never injected into environment
  variables. Requests use OpenAI-compatible chat completions through xAI's
  Grok CLI inference proxy with the required OAuth headers.
  Default model is `grok-4.5`. Model selection uses a static agent-ready catalog
  matching pi (`grok-4.5`, `grok-4.3`, `grok-build-0.1`) instead of live
  `GET /models`. `/login grok` can import credentials from the official Grok CLI
  (`~/.grok/auth.json`) when present. Setup wizard, `/backend`, `/doctor`, and
  `/auth` list the new provider.
- **Arrow-key selection menus.** Setup and the interactive `/backend` and
  `/model` flows now support ↑/↓, j/k, Home/End, Enter, number-key shortcuts,
  and q/Esc cancellation with windowed scrolling for long lists. `/model`
  retains its text filter, pricing/context labels, free-text fallback, and
  `(selected)` / `(default)` markers; interactive choices retain the save-as-
  default confirmation and direct `--default` commands remain supported.

## [1.2.5] - 2026-07-20

### Added

- **`/model --default` and `/backend --default`.** Pin the active (or newly
  selected) backend/model into `agent.config.json` without re-running `/setup`.
  `/model … --default` writes `backend` + `modelOverride`; `/backend … --default`
  writes `backend` and clears `modelOverride`. Session changes still apply if the
  file cannot be written; invalid JSON is never overwritten.
- **Default-aware `/model` and `/backend` pickers.** Interactive lists now mark
  each entry with `(selected)` when it is the live session choice and `(default)`
  when it is what `agent.config.json` would load on next launch (both show when
  they coincide). Append `--default` to a selection (e.g. `3 --default`) to pin
  it as you pick; otherwise a `y/N` prompt offers to save the choice as the
  project default.

## [1.2.4] - 2026-07-20

### Fixed

- **Aligned output after raw-mode prompts.** Sub-prompts now emit a carriage
  return with their newline before leaving raw mode, so confirmations and
  subsequent picker output start in column 0. Backend and model pickers also
  include spacing around the selection prompt for easier scanning.

## [1.2.3] - 2026-07-16

### Fixed

- **Compaction notice after dual-model failure.** When a dedicated compaction
  model fails and the main-model retry also fails (or a configured compaction
  backend is unavailable and the main model then fails), the harness still
  deterministic-trims older turns, but the compaction notice no longer claims
  the transcript was "summarized with" the main model. The warning now reports
  that both paths failed and that the transcript was trimmed.

## [1.2.2] - 2026-07-15

### Fixed

- **Conservative shell approvals.** `dangerous-only` now skips approval only
  for commands that Small Harness can clearly classify as non-destructive.
  Destructive Git operations, file-moving and in-place editing commands,
  redirects, interpreters, compound shell syntax, and unknown commands prompt
  instead of relying on a bypass-prone dangerous-command regex. Shell child
  processes also strip hydrated API-key env vars so approved or auto-run
  commands cannot casually exfiltrate `OPENAI_API_KEY` / `OPENROUTER_API_KEY`.

## [1.2.1] - 2026-07-14

### Added

- **Configurable compaction model.** `modelSystem.compaction` selects the model
  used to summarize the conversation during context compaction (both automatic
  compaction and `/compact`). It defaults to the main conversation model; set it
  to a `ModelRef` object (`{ "backend": "...", "model": "..." }`) to compact with
  a different (for example cheaper or longer-context) model. When the configured backend is not ready, compaction
  inherits the main model and prints a warning in the compaction notice; if the
  chosen model errors mid-compaction, it retries with the main model before
  falling back to the deterministic trim, reporting the failure instead of
  silently degrading. The configured model is shown in `/route status` and
  `/route template`.

## [1.2.0] - 2026-07-08

### Added

- **Complexity-aware plan routing.** `/plan route <intent>` can now ask a
  configured planning model to break a larger request into low-, medium-, and
  high-complexity tasks, persist the plan to `.small-harness/plan.json`, and
  route each ready task through the configured execution tier. `/plan status`
  shows task progress, and `/plan execute` runs ready tasks sequentially while
  switching models for each task.
- **Model-system planner tier.** `agent.config.json` can now define
  `modelSystem.planner` alongside selector and execution models, making it
  possible to plan with a subscription-backed or API-backed frontier model and
  execute subtasks across cheaper, local, or specialized models.
- **Claude Fable usage tracker.** `/fable` now shows weekly Small
  Harness-tracked Fable tokens, turns, share of tracked Claude-family usage,
  and optional remaining allowance for plans where Fable is capped to a share
  of weekly usage. Fable turns also append a compact weekly tracker to the
  footer.
- **Terminal visual polish.** The interactive UI now has streamed markdown
  styling, code-block framing, colored diffs, theme handling, a gradient
  banner, and `NO_COLOR`-aware output for cleaner long-running sessions.

### Fixed

- **Dependency audit.** Updated `quinn-proto` and `crossbeam-epoch` to versions
  that clear the current RustSec audit failures while preserving the existing
  allowed `anyhow` warning policy.

## [1.1.1] - 2026-07-02

### Added

- **Interactive external eval fixtures.** `/eval agent <fixture.json>` accepts
  the same external fixture JSON paths as the `--eval` CLI flag, reusing the
  same path-safety resolver.

## [1.1.0] - 2026-07-02

### Added

- **Hooks.** `agent.config.json` can define trusted command hooks for session,
  prompt, tool, permission, plan, stop, and session-end events. Hooks receive
  JSON on stdin, can block/allow/deny/stop, add context or feedback, and are
  traced quietly unless they warn or affect execution. Project hooks run only
  after their current hash is trusted in user-owned state; project-controlled
  hook state is ignored for execution safety. `/hooks` lists and manages trust
  state stored under `~/.config/small-harness/`.
- **Managed launch hooks.** Launchers can inject ephemeral trusted hooks with
  `SMALL_HARNESS_MANAGED_HOOKS_JSON` or
  `SMALL_HARNESS_MANAGED_HOOKS_FILE`, enabling terminal/orchestrator
  status and progress integrations without mutating user config. These are
  process-local launcher-trusted hooks, not cryptographically signed hooks or
  Codex enterprise-managed hooks. Hook commands can now use `envVars` to
  forward selected parent process variables and `env` for literal values,
  while keeping parent credentials absent by default.
- **Hook hardening.** Gating hook runner failures now fail closed, invalid
  matchers are visible but skipped, regex matchers use full-match semantics,
  `task` uses the dedicated subagent hook events without generic `PostToolUse`,
  hook subprocesses use process-group cleanup on timeout, and hook-provided
  model context is bounded and redacted before injection.
- **External eval fixtures.** `--eval` can now point at a data-only fixture
  JSON file in addition to the built-in fixture IDs. Fixture workspaces
  resolve relative to the fixture file, and workspace or `fileContains.path`
  refs that use absolute paths or parent traversal are rejected.

### Changed

- **Default Ollama model is now `qwen2.5:7b`** (was `qwen2.5-coder:7b`). The
  coder variant emits tool invocations as raw JSON text instead of using
  Ollama's structured `tool_calls` API, so sessions silently stalled on the
  first tool call. Users who want the coder fine-tune can still set
  `modelOverride` in `agent.config.json`.

### Fixed

- **Thinking-model rendering.** Streaming reasoning models (qwen3,
  deepseek-r1) no longer print a new `response` header for every reasoning
  token. Empty content deltas no longer open answer panels, and reasoning
  events no longer close the in-progress answer when reasoning display is
  off.

## [1.0.4] - 2026-06-20

### Added

- **Scorecard diagnostics.** `/scorecard doctor` inspects the local append-only
  scorecard ledger, reports valid turns/PRs, and lists skipped malformed JSONL
  lines. `/scorecard export [path]` copies the raw ledger before repair or
  sharing.
- **Scorecard remote verification.** `/scorecard verify <n>` fetches GitHub PR
  state, review decision, mergeability, and check-rollup status for a closed PR
  with a stored GitHub URL. `/scorecard verify --all` refreshes all verifiable
  recent PRs and appends verification events without mutating the original local
  close-time audit snapshot.

### Changed

- **Scorecard quality explanations.** Scored PR records now store explicit
  reasons when they do not count as quality-shipped, and `/scorecard pr <n>`
  renders those reasons next to the local evidence.
- **Safer scorecard reset.** `/scorecard reset --yes` now saves a timestamped
  backup before removing the active scorecard store.

## [1.0.3] - 2026-06-19

### Added

- **Scorecard quality loop.** Manual `/scorecard close` now runs shipcheck and
  scores PR units with the same readiness/test rubric as `/ship pr`. Supports
  `--url` and `--tests`. Closed PR records store session IDs and optional ship
  record paths. Turn footer and `/ship` preview nudge when a branch accumulates
  tracked turns. Configure via `scorecard.enabled`, `qualityThreshold`, and
  `nudgeMinTurns` in `agent.config.json`.
- **Scorecard audit trail.** PR close now snapshots per-session turn-trace
  summaries (turns, steps, tool calls, timing) from local `.events.jsonl` logs.
  `/scorecard prs` shows numbered rows; `/scorecard pr <n>` drills into quality
  evidence, sessions, and artifact paths captured at close time.

### Fixed

- **Manual scorecard closes.** `/scorecard close <label> --url <pr> --tests`
  can count as a quality PR when the local quality bar is met, even though the
  PR was opened outside `/ship pr`.
- **Turn trace JSON.** Event log timing fields now use `u64` milliseconds so
  turn summaries deserialize correctly from JSONL (fixes scorecard audit reads).

## [1.0.2] - 2026-06-18

### Added

- **Global quality PR scorecard.** `/scorecard` now rewards well-verified PRs
  instead of the lowest-token PRs. Small Harness still records successful
  interactive turn token usage globally, but `/ship pr` closes the current
  repo/branch with a quality snapshot from local ship readiness, test evidence,
  and PR creation state. The default scorecard reports quality PRs shipped,
  quality rate, average quality score, clean ships, follow-up count, and tokens
  per quality PR.

## [1.0.1] - 2026-06-15

### Added

- **Active effort routing.** `/route select` can now accept selector-chosen
  `coderEffort`, `reviewEffort`, and `securityEffort` values. The selected
  coder effort is applied to the live session, shown in `/session` and the turn
  footer, and sent to OpenRouter as `reasoning.effort` for models that support
  adjustable reasoning depth. Local backends keep the effort visible but ignore
  unsupported request fields.

## [1.0.0] - 2026-06-14

### Added

- **Multi-model routing with `/route`.** `modelSystem` in `agent.config.json`
  can define selector, orchestrator, coder, review, and security-review models
  across local and frontier backends. `/route select <task>` asks the selector
  to classify the task and applies the chosen coding model; `/route status`,
  `/route template`, and `/route apply ...` support inspection and manual
  switching.

## [0.9.0] - 2026-06-14

### Added

- **OpenRouter Fusion support.** `/fusion on` switches the active session to
  the `openrouter/fusion` alias for deliberative coding questions, while
  `/fusion tool [model]` keeps a chosen OpenRouter coding model as the outer
  agent and attaches OpenRouter's Fusion plugin for high-stakes reviews,
  architecture tradeoffs, and debugging. Fusion tool mode can be configured
  with `panel=...`, `judge=...`, and `max-tools=...`, or persisted under
  `openrouter.fusion` in `agent.config.json`.
- **OpenRouter-reported cost in the turn footer.** Streaming usage chunks now
  read `usage.cost` when OpenRouter returns it, so dynamic routers such as
  Fusion can show real per-turn and session cost instead of always falling back
  to `$?`.

## [0.8.0] - 2026-06-14

### Added

- **`/ship` preflight, local commit, push, PR, and status flow.** A last-mile
  shipping command that reuses the existing git/test readiness collectors,
  prints a ready / needs review / blocked verdict with concrete blockers, and
  drafts a commit message from the handoff context. `/ship commit --all` stages
  the working tree after confirmation; `/ship commit --staged-only` commits
  only already-staged files. `/ship push` pushes the current branch, using the
  configured upstream or setting `origin/<branch>` as upstream when needed.
  `/ship pr` creates a draft GitHub pull request through `gh pr create` and
  prints the exact fallback command when GitHub CLI is unavailable or
  unauthenticated. `/ship status` finds the open PR for the current branch and
  summarizes GitHub checks/review state through `gh pr list`. Successful
  commits, pushes, and PR attempts save records under `.sessions/ship/`. Use
  `/ship --tests` to include the project test suite; cloud backends keep
  diff-context drafting local unless `--cloud` is passed.

## [0.7.0] - 2026-06-09

### Added

- **Turn tracing and session event log.** Every turn appends structured events
  (tool calls with redacted args, approvals, output compaction, warmup, timing)
  to a sidecar at `.sessions/<session-id>.events.jsonl`, enabled by default via
  `display.eventLog.enabled`. `/trace on|off` shows nested subagent/critic tool
  calls as indented lines in the TUI — previously invisible — and
  `/export <session> events` copies the sidecar. The end-of-turn status line
  gains a timing breakdown (TTFT, model, tools, approval, total), the loader
  names the tool currently running, and compaction of oversized tool output is
  reported with the original size.
- **Agent eval CLI.** `small-harness --eval <fixture> [--model M] [--json]`
  runs a bundled eval fixture from the shell and exits 0 on pass / 1 on fail,
  for CI scripts. An optional `agent-eval` CI job runs two fixtures against
  Ollama nightly or on `[eval]` in a commit message (continue-on-error, so a
  flaky local model never blocks merges). New integration tests drive the real
  agent loop against a mock OpenAI-compatible SSE server — no live LLM needed.

### Fixed

- `file_edit` can create new files via the empty-`old_text` convention used by
  Claude Code and similar harnesses.
- Tool responses for three model-facing edge cases: `file_read` with an offset
  past EOF returns a clear error instead of silently-empty content, `list_dir`
  reports the real entry `total` when a listing is truncated, and `grep` drops
  unparseable ripgrep output lines instead of emitting malformed matches.
- The rubric heading parser matches `(weight:` case-insensitively on raw bytes,
  fixing potential mis-parses of criterion names containing certain Unicode
  characters.
- The HTTP client now uses a 10-second connect timeout so a dead backend fails
  fast instead of hanging, without capping long streaming completions.

### Changed

- Internal: the 3,000-line commands module was split into focused submodules
  (config, context, memory, session). No behavior change.

## [0.6.1] - 2026-06-07

### Added

- **`/plan validate`** — checks the spec's Done Criteria against the
  working-tree diff and prints a met/unmet checklist, closing the
  `/plan` → `/iterate` → `/auto` loop with a one-shot "am I done?" command. It
  reuses the same done-check `/auto` runs each round; like `/iterate` it sends
  the diff to the model, so it runs on a local backend unless `rubric.allowCloud`
  is set.

## [0.6.0] - 2026-06-07

### Added

- **`openai-codex` backend — sign in with ChatGPT/Codex.** A new backend that
  authenticates with a ChatGPT/Codex subscription via OAuth (Authorization Code
  + PKCE, with a device-code fallback) instead of an API key — run
  `/login openai-codex`. Access and refresh tokens are stored in the `0600`
  `auth.json` beside API keys, refreshed automatically on expiry, and never
  injected into environment variables. Requests route through a Codex Responses
  adapter; `OPENAI_CODEX_BASE_URL` overrides the endpoint. Existing
  `{"provider":"sk-..."}` auth files keep working unchanged. (Thanks
  @BlockedPath.)

### Security

- The Codex OAuth PKCE verifier and CSRF `state` nonce are generated from the OS
  CSPRNG (`getrandom`) on every platform, replacing a `/dev/urandom`-or-hash
  fallback that was predictable on systems without `/dev/urandom` (notably
  Windows).

## [0.5.0] - 2026-06-07

### Added

- **`/auto` — autonomous overnight run.** Chains the `/iterate` loop with
  automatic `/reset` so a multi-hour, unattended run drives a goal to "done"
  without blowing its context budget. Each round the generator works toward the
  goal and the `critique` evaluator scores the diff; when the context window
  fills past a ratio (`--reset-at`, default 0.75) it drafts a continuation
  handoff and starts a fresh session, carrying the goal and latest feedback
  forward. Takes an inline goal or `--spec` (reads `.small-harness/spec.md` from
  `/plan`, and checks its Done Criteria against the diff each round). Always
  finitely bounded — `--max N` (default 12, hard cap 40), an optional `--budget`
  on generator spend, and an optional `--deadline 6h` — and stops early on a
  stall (no score gain and no diff change for 3 rounds). Leaves a morning report
  at `.small-harness/auto-report.md` (verdict, per-round scores, criteria,
  cost, elapsed, resets) on every exit path, including Ctrl-C. Same refusals as
  `/iterate` (runs on a local backend unless `--cloud`; needs `rubric.enabled`;
  not during `/play`). Defaults configurable via the `auto` block
  (`maxRounds`, `budgetUsd`, `resetRatio`, `deadline`).

## [0.4.14] - 2026-06-06

### Added

- **`/plan` — spec expansion.** Expands a one- or two-sentence intent into an
  ambitious spec (goal, user outcomes, scope, done criteria, open questions)
  written to `.small-harness/spec.md`. Stays at the level of *what* and *why*,
  not implementation, so an early spec doesn't lock in the wrong details.
  `/plan show` prints it; `--export <path>` writes elsewhere.
- **`critique` evaluator + grading rubric.** A separate, read-only critic agent
  scores work 0–10 against a weighted rubric and returns actionable feedback.
  The harness — not the model — computes the weighted total and pass/fail, so a
  critic that over-rates can't pass weak work; the default rubric penalizes
  generic "AI slop". Override the criteria with a `.small-harness/rubric.md`
  using `## Name (weight: N)` sections. Configured via the `rubric` block; cloud
  backends require `rubric.allowCloud` before any workspace context is sent.
- **`/iterate` — generate→evaluate→improve loop.** Runs the generator, grades
  the diff with the `critique` evaluator, and feeds the feedback back —
  refining or pivoting — until the score clears the threshold or it runs out of
  rounds (`--max N`, default 6, capped at 15; `--threshold X`). Set
  `iterate.evaluatorModel` to grade with a different model than the generator.
- **Live verification.** With `rubric.liveVerify`, the critic runs the project's
  test suite — via a fixed-surface `verify` tool (no arbitrary shell,
  timeout-bounded) — before scoring functionality, instead of reading the code
  alone.
- **`/reset` — context reset with a handoff artifact.** Writes a structured
  continuation note (done / in progress / key decisions / next steps / key
  files) to `.small-harness/continue.md`, then starts a fresh session seeded
  with only that artifact — "reset over compaction" for coherence on long
  tasks. `/reset --dry-run` writes without clearing; cloud backends require
  `--cloud`.

## [0.4.13] - 2026-06-05

### Added

- **Navigable slash-command completion menu.** Typing `/` now opens a dropdown
  of matching commands with their descriptions beneath the prompt (the best
  match also shown as dim ghost text). **↑/↓** select, **Tab** accepts (with a
  trailing space), **→** accepts inline, **Esc** dismisses; **↑/↓** fall back to
  history when the menu is closed. Upgrades the inline-only completion from
  0.4.12.

## [0.4.12] - 2026-06-04

### Added

- **Inline tab-completion for slash commands.** Type `/` and the rest of the
  best-matching command appears as dim ghost text; **Tab** accepts it (with a
  trailing space), **Right** accepts it at end-of-line. Updates live as you
  type and never interferes with mid-line editing.

## [0.4.11] - 2026-06-04

### Added

- **`/verbose` mode** — a debug tool view. `/verbose on` prints every tool call
  as `→ name` with its full arguments and the result as `← (duration)` with a
  large preview, so you can see exactly what the agent is doing; `/verbose off`
  restores the normal grouped view.

## [0.4.10] - 2026-06-04

### Changed

- **The agent now writes files instead of printing code into the chat.** Three
  fixes together: (1) `auto` tool-selection no longer guesses tools from
  keywords — it keeps the full working pool available for any real request and
  sends nothing only for greetings, so prompts like "build a bio site" get the
  edit tools they need; (2) `file_write` is in the core defaults so new files
  can be created from scratch; (3) the system prompt instructs the model to
  write changes to disk and reply with a short summary rather than pasting file
  contents. No more token-wasting code dumps.

## [0.4.9] - 2026-06-04

### Added

- `/exit` and `/quit` slash commands to leave a session, so quitting is
  consistent with every other command. Bare `exit`/`quit` still work.

## [0.4.8] - 2026-06-04

### Changed

- Finished removing the profile concept: the internal `mac-mini-16gb` /
  `mac-studio-32gb` labels are gone from the recommender and no longer appear
  anywhere in the codebase or the compiled binary. Model recommendation scores
  off detected hardware tier + memory directly.

## [0.4.7] - 2026-06-04

### Changed

- **Removed the hardware-profile concept** (`mac-mini-16gb` / `mac-studio-32gb`).
  Each backend now has one sensible default model — override with `/model`,
  `AGENT_MODEL`, or `modelOverride`. The `/profile` command, the `profile`/
  `profiles` config fields, the `PROFILE` env var, and the banner profile line
  are gone. Existing configs with a `profile` key still load (it's ignored);
  model recommendation under `/doctor` still works off detected hardware.

## [0.4.6] - 2026-06-04

### Changed

- **Turn rendering redesigned.** Dropped the rounded response/input boxes in
  favor of a minimal header: a short top rule (~20% width) that fades from
  cyan to dark, with the `you`/`response` label and no bottom or side bars.
  Body text now wraps to the real terminal width so it fills the window like
  naturally-wrapped text rather than overflowing a fixed-width box.
- The raw-mode bordered input box was retired in favor of a `you` header plus
  the standard line reader (more robust, no half-drawn borders).

## [0.4.5] - 2026-06-03

### Fixed

- **Response text no longer overflows the panel.** Streamed assistant text is
  now word-wrapped to the panel's inner width (preserving paragraph breaks and
  list/code indentation, hard-breaking overlong URLs) instead of running to the
  full terminal width and spilling past the panel's right edge on wide
  terminals.
- The bordered input no longer erases its own bottom border on submit, so the
  `you` box stays a closed panel.

### Added

- The first-run **setup wizard** now matches the themed TUI: accent title +
  rule, bold section headers, readable (non-faint) text, and an accent `❯` on
  each prompt. Previously it was the one screen still using the old plain
  styling.

## [0.4.3] - 2026-06-03

No user-facing changes. Release-infrastructure only: tagging a release now
auto-updates the Homebrew tap formula (version + checksums) via the release
workflow, so `brew upgrade small-harness` tracks new releases automatically.

## [0.4.2] - 2026-06-03

### Fixed

- `small-harness --version` and `--help` now print and exit immediately
  instead of loading config and validating the backend — which previously
  errored with "OPENROUTER_API_KEY is required when BACKEND=openrouter" when
  no key was set. `--help` also gained a proper usage summary.

## [0.4.1] - 2026-06-03

A UI and onboarding polish pass.

### Added

- **TUI visual refresh.** A shared `theme` module drives a high-contrast
  palette and rounded-panel helpers. Input is now a framed `you` panel with an
  accent `❯` prompt; the assistant answer streams inside a framed `response`
  panel; tool calls use accent bullets, gutter-aligned. Crucially, secondary
  text no longer uses ANSI *faint* (the main reason the old TUI was hard to
  read) — it's readable bright-black.
- **API-key prompt in setup.** Choosing a cloud backend (`openai`/
  `openrouter`) in the wizard now prompts for the key and saves it to the
  `0600` auth file, so the end-of-setup probe no longer fails on a missing key.

### Changed

- The setup wizard no longer asks for a hardware profile (it keeps the
  existing/default profile silently).

### Docs

- README: a clear **Run it** section with the launch command, cloud and local
  presented as two equal paths, cleaner layout, and no hardcoded backend count.
- Moved internal AI-workflow checklists out of the public repo.

## [0.4.0] - 2026-06-03

A core-power + bare-bones pass: the agent loop got faster and more capable,
and the command/tool surface got leaner.

### Added

- **Parallel read-tool execution** — read-only tools (`file_read`, `grep`,
  `list_dir`, `glob`, `repo_search`, `ship_status`) emitted in a single step
  now run concurrently. Mutations and `shell`/`run_tests`/MCP stay serial;
  approval and checkpoint capture remain sequential.
- **`update_plan` tool** — the model maintains a short, visible task checklist
  across a multi-step turn (rendered as a plan box). No new slash commands; the
  plan lives in the conversation and auto-selection only surfaces it for
  multi-step work.
- **`task` subagent tool** — delegates a scoped, read-only investigation to a
  fresh agent loop (capped at 12 steps, no edit/shell, no recursion) and returns
  only its summary, keeping deep exploration out of the parent context.
- **Step-budget signalling** — when the loop exhausts `max_steps` mid-task it now
  sets `RunResult.hit_step_limit`, emits `StepLimitReached`, and shows a
  resumable notice instead of stopping silently. Eval results record it too.
- **Edit verification** — `file_edit` re-reads from disk after writing, returns
  `verified` (disk matches the intended write) and a line-numbered
  `applied_snippet` of the changed region so the model confirms the edit landed.
- **Session paths** — terminal-native branching without a worktree.
  `/path fork [name]` snapshots the workspace and clones the transcript;
  `/path switch <name>` restores another path; `/path diff` and `/path pick`
  compare and merge file changes with approval preview. State lives under
  `.sessions/paths/<session-id>/`. Config block `paths` controls enablement,
  max paths, and snapshot byte caps. Status line shows `path: name · N paths`
  when multiple paths exist. Resumes via `--continue`, `/resume`, and session
  metadata `activePathId`.

### Changed

- **Model-tuning commands folded under `/doctor`** — `/doctor` is now the single
  entry point with subcommands `recommend`, `autotune`, `bench`, and `models`.
  The old top-level `/recommend`, `/autotune`, `/bench`, and `/capabilities`
  print a one-line redirect and no longer appear in `/help`.
- **Default tool pool** is now `file_read`, `grep`, `list_dir`, `file_edit`,
  `shell`, `update_plan`, `task`. `shell` is on by default (still
  approval-gated); `repo_search` moved to opt-in.
- **`commands.rs` split** into `commands/{mod,doctor,workflow}.rs` with `dispatch`
  kept as a thin router. No behavior change.

## [0.3.0] - 2026-05-24

A polish + capability pass. Direct OpenAI provider, per-turn cost on the
status line, persistent credential store, an MCP client, image input,
`web_fetch`, a per-project system prompt, `--continue`, completions, a
GitHub release check, redacted crash logs, and a notarization-ready
release workflow with a Homebrew formula template.

### Added

- Direct OpenAI backend (`BackendName::OpenAi`, default model `gpt-4o-mini`,
  `OPENAI_API_KEY` / optional `OPENAI_BASE_URL`). `BackendName::is_local()`
  generalizes the cloud check so handoff refusal, recommend filtering, and
  capability scoring stay correct as cloud backends are added.
- Per-model catalog (`src/catalog.rs`) with context window and input/output
  USD-per-Mtoken for OpenAI's GPT-4o family, o-series, GPT-4, GPT-3.5.
  Lookup does exact-match then longest-prefix so versioned ids like
  `gpt-4o-2024-11-20` resolve to `gpt-4o`. `/model` picker shows
  `128k ctx · $0.15/$0.60 per Mtoken` alongside each id.
- Session cost tracker: end-of-turn status line shows `$0.003 this turn ·
  $0.41 session` on cloud backends. Local turns show tokens only. Sessions
  that mix cloud and uncataloged turns prefix the total with `≥` to mark
  it as a lower bound.
- `/auth` command + persistent credential store at
  `~/.config/small-harness/auth.json` (mode `0600`, masked display, env
  vars win at lookup time).
- `/image <path>` attaches images to the next user turn via OpenAI's
  multi-part content format. New `UserContent` enum is `#[serde(untagged)]`
  so plain text turns keep the existing wire shape. Catalog now tracks
  `vision: bool` per model.
- MCP client (`src/mcp.rs`) over stdio JSON-RPC with
  `initialize`/`tools/list`/`tools/call`. Servers configured under
  `mcpServers` in `agent.config.json` are spawned at startup and their
  tools surface as `mcp__<server>__<tool>`, approval-gated by default.
- `web_fetch` tool — approval-gated, HTML-stripped to text, byte-capped.
- Per-project system prompt: drop `.small-harness/prompt.md` at the repo
  root and Small Harness prepends it (auto-truncated at 8 KB).
- `small-harness --continue` (or `-c`) resumes the most recent session in
  the cwd without picking from a list.
- Background GitHub release check with a 24h cache; one-line banner notice
  if a newer version exists. Opt out with `SMALL_HARNESS_NO_UPDATE_CHECK`.
- Crash log: panic hook writes a redacted log (API keys scrubbed) to
  `.sessions/crashes/<timestamp>.log` and prints the path.
- `small-harness completions bash|zsh|fish` emits shell completion
  scripts.
- `/reasoning on|off|status` toggles the streaming reasoning panel with a
  proper "thinking…" header.
- GitHub Actions release workflow at `.github/workflows/release.yml` —
  tag-push triggered, dual-arch macOS build, optional sign + notarize when
  Apple Developer secrets are present, GitHub release with SHA256SUMS.
- Homebrew formula template at `packaging/homebrew/small-harness.rb`
  targeting that workflow's artifacts, ready to drop into a
  `getsmallai/homebrew-tap` repo.

### Changed

- Refactored `BackendName::Openrouter`-as-cloud checks in `handoff.rs`,
  `recommend.rs`, `capabilities.rs`, and `commands.rs` to route through
  `BackendName::is_local()` instead.
- `/new` now resets `total_in`, `total_out`, `session_usd`, and the
  uncataloged-cost flag so a fresh session genuinely starts fresh.
- Tagline and masthead updated: "A small, terminal-first coding harness —
  bring your own model, your own key, or your own MCP server." README
  fully restructured around install → first session → reference flow.

## [0.2.x predecessors]

Earlier `0.2.x` work that landed before 0.3.0 includes: interactive
`/play` playground with bundled demos and scorecards, `/fix` fix-until-green
loop, shared `session_turn.rs` runner, agent workflow tools (`run_tests`,
`batch_edit`, `ship_status`), `/mode ship` autopilot, agent eval suite,
turn checkpoints with `/undo`, multi-file `/batch` and `/refactor`, the
`/test` command, the `/prompt` template library, and the adaptive context
guard with auto-compaction.

## [0.2.2] - 2026-05-03

### Added

- `/handoff [export|save] [path] [--cloud]` for local model-drafted commit
  messages, changelog bullets, testing notes, and X-ready release copy from the
  current git branch or working tree.

### Changed

- Fixed the Quick Install clone URL to point at `GetSmallAI/SmallHarness`.

## [0.2.1] - 2026-05-02

### Added

- `/shipcheck [export [path]]` release preflight for git branch drift, staged
  and unstaged changes, untracked files, conflicts, diff stats, and
  project-memory freshness reports.

## [0.2.0] - 2026-04-29

### Added

- Project memory freshness checks with `/index status`, changed-file refresh,
  and automatic refresh after successful file mutation tools.
- Operator modes with `/mode explore`, `/mode edit`, `/mode ship`, and
  `/mode review` presets for tool sets, approvals, and step budgets.
- Session lifecycle improvements: inferred/settable titles, `/sessions search`,
  confirmed session deletion, and safe pruning of old sessions.
- Non-interactive one-shot mode via `--print`, `-p`, or piped stdin, with
  explicit `--allow-tools` approval for write/shell tool execution.
- Reasoning stream support for OpenAI-compatible chunks that expose
  `reasoning`, `reasoning_content`, or `thinking` deltas.

### Changed

- Bumped the product line to `0.2.0` for the broader release scope.

### Tests

- Added mock OpenAI-compatible SSE coverage for streamed tool-call deltas.

## [0.1.34] - 2026-04-29

### Added

- Local project memory under `.sessions/project-memory/`, with a metadata-only
  repo index that honors `.gitignore`, skips secret/env files, binaries,
  oversized files, `.git`, `.sessions`, `target`, and `node_modules`.
- `/index`, `/map`, `/memory`, `/remember`, and `/forget` commands for building,
  inspecting, toggling, and annotating project memory.
- `repo_search` tool for ranked indexed-file lookup with symbols, headings,
  imports, reasons, and short on-demand snippets.
- Smart local repo-map injection for repo/code prompts, disabled for cloud
  backends unless `projectMemory.allowCloudContext` is explicitly enabled.
- Deterministic extraction for Rust, Python, TypeScript/JavaScript, Markdown,
  JSON, TOML, and generic text files.

## [0.1.33] - 2026-04-28

### Added

- `/recommend [refresh] [all] [--cloud] [apply]` for hardware-aware model
  recommendations tuned for local coding-agent use.
- Safe hardware summary detection for OS, architecture, chip, machine name,
  memory, and CPU counts, cached under `.sessions/hardware.json` without raw
  serials, UUIDs, or UDIDs.
- Model candidate parsing for parameter size and quantization hints, memory-fit
  scoring, installed/default/cached candidate ranking, and explicit apply
  behavior for the active session.
- First-run setup now uses detected hardware memory to choose the default
  existing hardware profile.

## [0.1.31] - 2026-04-27

### Added

- Persistent capability cache under `.sessions/capabilities/` for per-backend
  and per-model probe results, tool-call support, inline JSON fallback support,
  usage chunk support, warnings, and benchmark stats.
- `/capabilities [refresh] [all]` to view the cached model scoreboard or refresh
  active/all backend probes.
- `/autotune [refresh] [all] [--cloud] [apply]` to score cached models, explain
  the best fit, and optionally apply the recommended backend/model/tool mode to
  the active session.
- `/doctor --deep` and `/bench` now feed the capability cache.

## [0.1.30] - 2026-04-26

### Added

- First-run setup wizard that writes `agent.config.json`, chooses backend,
  hardware profile, model override, approval policy, and tool-selection mode,
  probes the selected backend, and can be rerun with `/setup`.
- Documented pre-1.0 commit-count versioning, starting with `0.1.30` for the
  setup release commit.
- `llamacpp` backend support for `llama-server` OpenAI-compatible endpoints,
  including `LLAMACPP_BASE_URL`, optional `LLAMACPP_API_KEY`, backend switching,
  docs, and startup troubleshooting hints.
- `/doctor --deep [all]` capability probing for model listing, streaming chat,
  usage chunks, native tool calls, inline JSON fallback, llama.cpp `--jinja`
  warnings, and saved JSON/Markdown reports under `.sessions/doctor/`.
- Efficiency mode with adaptive tool-schema selection (`toolSelection:
  "auto"` or `/tools auto`), prompt-budget breakdowns in `/context`, prompt
  budget warnings, prompt-cache re-warm fingerprints, and compacted large tool
  outputs.
- `.env` and `.env.local` config loading with process environment variables
  remaining highest priority.
- Workspace path policy (`workspaceRoot`, `outsideWorkspace`) for file tools
  and shell execution.
- Diff-first approval previews for `file_edit`, `file_write`, and the new
  approval-gated `apply_patch` tool.
- Session commands: `/sessions`, `/resume`, and `/export`.
- Input history persisted under `.sessions/history.jsonl`, arrow-key recall,
  cursor movement, and Ctrl-J multi-line prompts.
- Ctrl-C cancellation for active model streams and shell commands.
- Context and operations commands: `/config`, `/context`, `/compact`,
  `/doctor`, `/bench`, and `/eval`.
- Custom profile model maps in `agent.config.json`.
- Unit coverage for dotenv parsing, session loading, history persistence,
  workspace policy, and patch application.

## [0.1.0] - 2026-04-25

First Rust release. Full port of the original TypeScript implementation with
feature parity and a few quality-of-life additions.

### Added

- Rust binary (`small-harness`) replacing the Node/TypeScript entry point
- `SseParser` struct for incremental parsing of OpenAI-compatible
  Server-Sent-Events streams (decoupled from the HTTP layer for testability)
- Hand-rolled `reqwest` + SSE chat-completions client (no SDK dependency)
- Trait-object tool registry with seven tools: `file_read`, `file_write`,
  `file_edit`, `glob`, `grep`, `list_dir`, `shell`
- Tool-call reassembly across streamed chunks
- Inline-JSON tool-call fallback for small models that emit tool calls as
  plain text content instead of populating `tool_calls`
- Approval gate with three policies (`always`, `never`, `dangerous-only`) and
  per-tool / per-call session caching
- All four backends: Ollama, LM Studio, MLX, OpenRouter
- Hardware profiles: `mac-mini-16gb`, `mac-studio-32gb`
- Slash commands: `/help`, `/new`, `/clear`, `/session`, `/backend`,
  `/profile`, `/model`, `/tools`, `/compare`
- Bordered + plain TUI input modes, three loader styles, four tool-display
  modes, ASCII banner
- JSONL append-only session persistence under `.sessions/`
- Pre-warm at startup to populate the prompt-eval cache
- Unit tests (47): SSE parser, tool-call detection regex, inline-JSON fallback,
  unified diff, base64, ignore filter, dangerous-command regex, async tool
  execute paths against `tempfile`-backed directories
- GitHub Actions CI: build + test on Ubuntu and macOS; clippy + fmt-check
  enforcement

### Removed

- TypeScript implementation (`src/*.ts`, `package.json`, `tsconfig.json`,
  `node_modules/`)
- Node and `tsx` runtime dependency
