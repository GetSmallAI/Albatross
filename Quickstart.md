# Albatross Quickstart

This guide is for the first 20 minutes with Albatross. It focuses on the
top things you can do immediately: try a bundled demo, fix failing tests on
your repo, understand a codebase, make a safe edit, and tune Albatross to
the best model available on your machine.

## Before You Start

You need Rust and at least one OpenAI-compatible local backend. Ollama is the
fastest first path:

```bash
brew install ollama
brew services start ollama
ollama pull qwen2.5-coder:7b
```

Then run Albatross from the project you want to work in:

```bash
cargo run --release
```

On first run, the setup wizard creates `agent.config.json`. For the quickest
local setup, choose:

```text
backend: ollama
model override: blank
approval policy: dangerous-only
tool mode: auto
```

## 0. Try It In 60 Seconds

Pull a 7B coder and run a bundled demo — no repo setup required:

```text
/play
/play fix-failing-test
```

Albatross copies a tiny Rust crate with a failing test into
`.sessions/play/`, switches to ship mode, and runs the agent live. You approve
edits (or pass `--yolo` to auto-approve). When it finishes, you get a scorecard
showing whether tests pass.

```text
/play score
/play exit
```

Then try the same loop on your real project:

```text
/fix
/fix all --attempts 3
/fix --yolo
```

`/fix` runs smart-selected tests, loops until they pass (default 5 attempts),
then restores your previous operator mode.

Compare two local models on the same demo:

```text
/play battle fix-failing-test qwen2.5-coder:7b,deepseek-coder:6.7b
```

## 1. Understand A Codebase Fast

Albatross is most useful when you let it inspect files directly instead of
pasting code into chat. Start with a broad map, then ask narrower questions.

Build the local project memory index first:

```text
/index
/index status
/map
```

Try:

```text
Give me a concise map of this repo. Focus on the entry points, core modules,
and where configuration lives.
```

Then:

```text
Find the code path for slash commands and explain how a new command should be
added.
```

Useful commands:

```text
/config       show the active backend, model, tools, workspace, and history
/mode explore use a safer read/search preset while learning a repo
/tools        show enabled tools and whether adaptive tool selection is on
/context      show prompt budget, effective limit, headroom, and auto-guard status
/map          show the local project memory repo map
```

What to look for:

- Albatross should use read/search/list tools only when needed.
- For repo/code questions, `repo_search` should help it find likely files fast.
- With `toolSelection: "auto"`, ordinary chat should avoid sending tool schemas.
- The answer should cite concrete files and functions, not just guess.

## 2. Make A Safe Local Edit

Albatross can edit files, but the best workflow is to ask for a small,
reviewable change and let approvals show you exactly what will happen.

Try:

```text
Add a short comment above the function that dispatches slash commands explaining
that new commands should be registered in both COMMANDS and dispatch.
```

Then inspect what happened:

```bash
git diff
cargo test
```

Useful commands:

```text
/mode edit    use edit-focused defaults
/mode ship    enable edit + workflow tools; auto-verify tests after edits
/shipcheck    show branch drift, dirty files, diff stats, and memory freshness
/ship         preview last-mile readiness, blockers, and a commit-message draft
/ship commit --all
/ship push
/ship pr
/ship status
/scorecard    show global quality PRs shipped
/scorecard pr 1   drill into the most recent closed PR audit
/scorecard close "OAuth login PR" --url https://github.com/org/repo/pull/42
/scorecard doctor   inspect the local scorecard ledger
/scorecard export   copy the raw scorecard JSONL before repair or sharing
/fable       show Claude Fable weekly usage and cap headroom
/handoff      draft commit, changelog, testing, and X-ready release copy
/fusion on    switch to OpenRouter Fusion for deliberative coding questions
/fusion tool  attach Fusion deliberation to an OpenRouter coding model
/route select choose low/medium/high models for a task from your model stack
/route explain show why the latest route was chosen and what actually ran
/route spend aggregate routed cost by role and resolved model
/session      show current model, approval policy, session file, and tokens
/session title Refactor dispatch command
/sessions search dispatch
/new          start a clean conversation
/export current markdown
/export current events   copy the session event log sidecar
```

**Transparent mode** (see everything the agent did):

```text
/verbose on
/trace on
```

The event log lives beside each transcript:
`.sessions/<session-id>.events.jsonl` (tool calls, approvals, compaction,
warmup, per-turn timing summary).

Good habits:

- Ask for one focused change at a time.
- Prefer exact files, functions, or tests when you know them.
- Keep `approvalPolicy` at `dangerous-only` or `always` until you trust a model.
  `dangerous-only` allows clearly non-destructive inspection and test commands
  without a prompt, while uncertain shell syntax and mutating commands still
  require approval. Shell child processes also strip hydrated API-key env vars.
- Use `git diff` as the source of truth before committing.

## 3. Tune The Best Local Model

Different local models vary a lot. Albatross can probe model capabilities,
cache the results, benchmark latency, and recommend the best cached fit.

Everything lives under `/doctor`. Start with a hardware-aware recommendation:

```text
/doctor recommend
```

This reads a safe summary of your Mac, ranks installed/default/cached models for
coding-agent use, and shows the top choices. To refresh probes before ranking:

```text
/doctor recommend refresh
```

Run:

```text
/doctor --deep
/doctor bench
/doctor models
```

If you have multiple backends running, probe them all:

```text
/doctor models refresh all
```

Then ask for a recommendation:

```text
/doctor autotune
```

Apply the recommendation to the current session:

```text
/doctor recommend apply
```

What Albatross is checking:

- local chip, architecture, memory, and CPU counts
- model listing
- streaming responses
- usage chunks
- native tool calls
- inline JSON fallback for small models
- first-token latency
- estimated output tokens per second

By default, `/doctor recommend` prefers local backends. To let OpenRouter
compete with local models, use:

```text
/doctor recommend --cloud
```

Long coding sessions on small local models can fill the context window quickly.
Albatross auto-compacts older turns on local backends when usage crosses
~85% of the effective limit (run `/context` to see headroom). Compaction keeps
complete tool-call rounds intact so the transcript stays valid for the next
request. Use `/compact` manually if you want to shrink sooner. One-shot
`--print` mode does not auto-compact.

## Scripts And CI

Use one-shot mode when you want Albatross without the interactive TUI:

```bash
cargo run --release -- --print "Summarize the repo entry points"
printf 'What changed in this branch?\n' | cargo run --release --
```

Approval-gated write and shell tools are denied in one-shot mode unless you pass
`--allow-tools`.

Run a bundled agent eval from the shell (exit code 0 on pass):

```bash
cargo run --release -- --eval read-and-explain --model qwen2.5-coder:7b
cargo run --release -- --eval fix-failing-test --json
```

You can also point `--eval` at a data-only fixture JSON file. Workspaces are
resolved relative to that fixture file and may not escape that root:

```json
{
  "id": "external-readme-check",
  "prompt": "Update README.md so it mentions the release version.",
  "workspace": "workspace/readme-check",
  "checks": [
    { "type": "fileContains", "path": "README.md", "needle": "version" }
  ]
}
```

```bash
cargo run --release -- --eval ./evals/local/external-readme-check.json --json
```

The interactive agent eval command accepts the same fixture path:

```text
/eval agent ./evals/local/external-readme-check.json
```

## A Good First Session

Here is a simple sequence that exercises the whole product:

```text
/config
/mode explore
Give me a concise map of this repo.
/index status
/doctor --deep
/doctor bench
/doctor recommend
/doctor models
/doctor autotune
Find one small README improvement and propose the exact diff before editing.
```

After the edit:

```text
/mode ship
Fix the failing test and get this ready to commit.
```

In ship mode the harness:

- injects a compact ship-status line into the system prompt each turn
- exposes `run_tests`, `batch_edit`, and `ship_status` as agent tools
- after a successful edit turn, runs smart-selected tests and injects failures into the next turn context (no automatic re-run loop)
- saves a turn checkpoint when files change — use `/undo` if the model breaks something

If a small model makes a bad edit:

```text
/undo
/undo list
/checkpoints status
```

`/undo` restores file contents from immediately before the last mutating agent turn and removes files the model created. Checkpoints are enabled by default in edit and ship modes.

You can still run the operator commands manually:

```text
/shipcheck
/shipcheck export
/ship --tests
/ship commit --all
/ship push
/ship pr
/ship status
/handoff
/handoff export
/fusion on
/fusion tool anthropic/claude-sonnet-4.5
/fusion off
/route template
/route select --dry-run add OAuth login and tests
/route apply coder high
/route explain
/route history 10
/route spend
/session
/test smart
```

`/ship commit` is local-only in this release: it can stage and commit after an
explicit confirmation, then saves a ship record under `.sessions/ship/`. It does
not create a PR by itself. `/ship push` pushes the current branch after
confirmation and sets `origin/<branch>` as upstream when the branch does not
have one. `/ship pr` uses GitHub CLI to open a draft PR, or prints the exact
`gh pr create` command if `gh` is missing or unauthenticated. `/ship status`
uses GitHub CLI to summarize the open PR, checks, review state, and next action
for the current branch.

Compare local models on agent-loop coding tasks:

```text
/eval agent fix-failing-test ollama:qwen2.5-coder:7b
/eval agent all
```

Then run:

```bash
git diff
cargo fmt --all -- --check
cargo test
```

## Where Things Are Saved

Albatross keeps local state under `.sessions/`:

```text
.sessions/
  history.jsonl          input history
  *.jsonl                session transcripts
  *.events.jsonl         per-session structured event logs (tools, timing, approvals)
  project-memory/
    index.json           safe metadata-only repo index
    notes.jsonl          durable project notes from /remember
  doctor/                deep doctor JSON and Markdown reports
  evals/                 eval suite JSON and Markdown reports
  shipcheck/             release-readiness Markdown reports
  handoff/               local ship-handoff Markdown drafts
  hardware.json          safe hardware summary, without serials or UUIDs
  capabilities/          per-model capability and benchmark cache
```

That local cache powers `/doctor recommend`, `/doctor models`, `/doctor autotune`, `/map`, and
`repo_search`.
