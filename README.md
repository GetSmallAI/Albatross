<h1 align="center">Small Harness</h1>

<p align="center">
  <strong>A small, terminal-first coding harness. Bring your own model, your own key, or your own MCP server.</strong>
</p>

<p align="center">
  <a href="#install">Install</a> &middot;
  <a href="#run-it">Run it</a> &middot;
  <a href="#first-session">First session</a> &middot;
  <a href="#backends">Backends</a> &middot;
  <a href="#tools-and-commands">Tools &amp; commands</a> &middot;
  <a href="#cost-and-credentials">Cost &amp; credentials</a> &middot;
  <a href="#going-further">Going further</a> &middot;
  <a href="#configuration">Configuration</a> &middot;
  <a href="#troubleshooting">Troubleshooting</a>
</p>

<p align="center">
  <a href="https://github.com/GetSmallAI/SmallHarness/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/GetSmallAI/SmallHarness/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="Rust" src="https://img.shields.io/badge/Rust-1.75%2B-dea584">
  <img alt="Version" src="https://img.shields.io/badge/version-1.2.7-111827">
  <img alt="Backends" src="https://img.shields.io/badge/backends-Ollama%20%7C%20LM%20Studio%20%7C%20MLX%20%7C%20llama.cpp%20%7C%20OpenRouter%20%7C%20OpenAI%20%7C%20Grok-2563eb">
  <img alt="Apple Silicon" src="https://img.shields.io/badge/Apple%20Silicon-optimized-111827">
  <img alt="License MIT" src="https://img.shields.io/badge/license-MIT-111827">
</p>

---

## What it is

A coding agent that lives in your terminal. Bring your own **API key**
(OpenAI or OpenRouter) or point it at a **local model** (Ollama, LM Studio,
MLX, llama.cpp) — same tools, same commands, same session log either way. It
ships with the usual tool kit — read, edit, grep, shell, run tests — plus a
few that aren't usual:

- **Local or cloud, one TUI.** Switch providers mid-session with
  `/backend <name>` — the tools, commands, and session log don't change.
- **Per-turn cost on the status line.** `$0.003 this turn · $0.41 session`
  when pricing is known or reported by the provider. Local turns just show
  tokens.
- **OpenRouter Fusion, one command away.** `/fusion on` switches to the
  `openrouter/fusion` alias for deliberative work; `/fusion tool` attaches
  Fusion to a chosen OpenRouter coding model for hard reviews, architecture
  tradeoffs, and high-stakes debugging.
- **Multi-model routing.** `/route select <task>` asks a configured selector
  model to pick low/medium/high orchestrator and coder tiers, plus play vs
  production review and security review, then switches the active coding model.
- **Routed plans.** `/plan route <goal>` asks a configured planner model to
  break work into a low/medium/high task graph, saves it to
  `.small-harness/plan.json`, and `/plan execute` runs ready tasks through the
  configured coder tiers.
- **Real undo.** `/undo` reverts the last agent turn's file mutations,
  including files the agent created or files that weren't tracked when the
  turn started.
- **Session paths.** `/path fork` branches the conversation and workspace so
  you can try two fixes, diff them, and `/path pick` the winner — no
  worktree required.
- **Plan, then grade the work.** `/plan` expands a one-line intent into a
  spec; `/iterate` runs a generate→evaluate loop where a *separate* critic
  agent scores each pass against a rubric and feeds back until it clears the
  bar — the generator never grades itself.
- **Reset over compaction.** `/reset` writes a handoff artifact and starts a
  clean session seeded with it — better coherence on long tasks than
  summarizing in place.
- **MCP-native.** Drop servers into `mcpServers` in your config; their tools
  show up as `mcp__<server>__<tool>` to the model on next launch.
- **`/auth` instead of `.env`.** Paste API keys once into a `0600` file
  under `~/.config/small-harness/`. Env vars still win when set.
- **Approval gates you can live with.** Every mutating call shows you the
  diff first, with `allow once / allow session / always allow` caching.

---

## Install

**Homebrew (macOS):**

```bash
brew install getsmallai/tap/small-harness
```

**From source** (Rust 1.75+):

```bash
git clone https://github.com/GetSmallAI/SmallHarness.git
cd SmallHarness
cargo build --release    # binary at target/release/small-harness
```

---

## Run it

Launch the interactive session:

```bash
small-harness
```

> From a source checkout without installing, use `cargo run --release` instead.

The first launch runs a short setup wizard (it writes `agent.config.json` —
backend, model, approval policy). Skip it with `SMALL_HARNESS_NO_WIZARD=true`.
Every launch after that opens straight into a session.

Small Harness talks to **one backend at a time** — pick the path that fits.

### Path A — Cloud API key

*Fastest to start, frontier-model quality, nothing to install locally.*

1. Set your key — OpenAI **or** OpenRouter:

   ```bash
   export OPENAI_API_KEY=sk-...
   # or
   export OPENROUTER_API_KEY=sk-or-...
   ```

2. Launch, then select the provider in the first-run wizard (or any time with
   `/backend openai`):

   ```bash
   small-harness
   ```

Prefer not to put the key in your environment? Launch first, then run
`/auth set openai` inside the app and paste it once — it's stored in a `0600`
file under `~/.config/small-harness/`. Cost per turn and per session shows
live on the status line.

### Path A2 — ChatGPT / Codex subscription login

If you want to use a ChatGPT/Codex subscription instead of OpenAI API billing,
log in with OAuth inside the TUI:

```text
/login openai-codex
/backend openai-codex
```

This is intentionally separate from `/auth set openai`: `openai` uses an
`OPENAI_API_KEY` and the public OpenAI API, while `openai-codex` stores a
refreshable ChatGPT OAuth token in `auth.json` and talks to the Codex Responses
backend.

### Path A3 — Grok / SuperGrok subscription login

Use a SuperGrok or X Premium+ subscription through browser or headless
device-code OAuth, with no `XAI_API_KEY`:

```text
/login grok
/backend grok
```

Pick **1) Browser login** (opens the system browser + local callback) or
**2) Device-code login** for SSH/headless. Tokens land in `auth.json` under
`grok` and refresh automatically. Default model is `grok-4.5`. `/model` exposes
the static agent-ready catalog (`grok-4.5`, `grok-4.3`, `grok-build-0.1`), same
as pi — it does not scrape xAI's full `/models` list.

### Path B — Local model

*Private, free, offline — runs entirely on your machine.*

1. Install [Ollama](https://ollama.com), start it, and pull a coding model:

   ```bash
   brew install ollama
   brew services start ollama
   ollama pull qwen2.5-coder:7b
   ```

2. Launch — Ollama is the default backend, so there's nothing else to set:

   ```bash
   small-harness
   ```

LM Studio, MLX, and llama.cpp work the same way — see [Backends](#backends)
for their ports and start commands.

> **Tip:** switch backends mid-session with `/backend <name>`, and run
> `/doctor` if a backend won't connect.

---

## First session

```text
> what files are in src/?

  Listed src/  (24 files)

src/ has 24 Rust files: main.rs is the entry point (input loop, banner,
warmup); agent.rs runs the chat-completions loop; backends.rs handles the
providers; tools/ contains the tool implementations…

  1.2k in · 87 out · $0.0003 this turn · $0.0003 session

> add a function in src/util.rs that lowercases a string and trims it

  Read src/util.rs
  Edited src/util.rs

  --- src/util.rs
  +++ src/util.rs
  @@ ...
  +pub fn normalize(input: &str) -> String {
  +    input.trim().to_lowercase()
  +}

  Apply? [y/n/a]: y
  checkpoint saved (1 file) — /undo to revert
  3.4k in · 412 out · $0.001 this turn · $0.0013 session
```

A handful of moves worth knowing right away:

- `/mode explore | edit | ship | review` toggles tool + approval + step-budget
  presets.
- `/undo` reverts the last turn's file mutations.
- `/path fork` branches the session to try an alternate approach; `/path switch`,
  `/path diff`, and `/path pick` compare and merge paths.
- `/shipcheck` summarizes git state; `/handoff` drafts a commit message,
  changelog bullets, and a release post from local context.
- `/ship` turns that into a last-mile preflight, local commit, and push path:
  readiness verdict, blockers, commit-message draft, guarded `git commit`, and
  guarded `git push`; `/ship pr` opens a draft pull request through GitHub CLI
  when available, and `/ship status` summarizes open PR checks/review state.
- `/scorecard` shows global quality PRs shipped; `/ship pr` closes a PR unit
  with readiness/test evidence. `/scorecard close <label>` scores manual closes
  from shipcheck (not the separate `/play score` fixture report).
- `/plan <intent>` drafts a spec; `/iterate <goal>` runs a generate→evaluate
  loop where a separate critic grades each pass against a rubric.
- `/play fix-failing-test` runs a bundled demo in an isolated sandbox so you
  can try a real agent loop without touching your repo.
- `Ctrl-J` for newline; `Enter` submits.
- `small-harness --continue` resumes the most recent session in the cwd.

---

## Backends

| Backend | Default URL | Notes |
|---------|-------------|-------|
| `ollama` | `http://localhost:11434/v1` | Easiest setup; mature tool-call templates |
| `lm-studio` | `http://localhost:1234/v1` | GUI model browser; explicit load / unload |
| `mlx` | `http://localhost:8080/v1` | Fastest inference on Apple Silicon (via `mlx_lm.server`) |
| `llamacpp` | `http://localhost:8080/v1` | Direct GGUF serving (via `llama-server`) |
| `openrouter` | `https://openrouter.ai/api/v1` | Cloud A/B with `/compare`; access to frontier models and Fusion |
| `openai` | `https://api.openai.com/v1` | Direct provider access with your own key |
| `openai-codex` | `https://chatgpt.com/backend-api/codex/responses` | ChatGPT/Codex subscription OAuth via `/login openai-codex` |
| `grok` | `https://cli-chat-proxy.grok.com/v1` | SuperGrok / X Premium+ OAuth via `/login grok` (browser or device-code) |

Switch at runtime with `/backend <name>`. Endpoint overrides:
`OLLAMA_BASE_URL`, `LM_STUDIO_BASE_URL`, `MLX_BASE_URL`, `LLAMACPP_BASE_URL`,
`OPENAI_BASE_URL`, `OPENAI_CODEX_BASE_URL`. The Grok OAuth proxy is fixed to
xAI's first-party host so subscription tokens cannot be redirected elsewhere.
API backends require an API key (set via [`/auth`](#cost-and-credentials) or
env var); `openai-codex` requires `/login openai-codex`; `grok` requires
`/login grok`.

### Default model per backend

Each backend has one sensible default; local backends default to a 7B coder
that runs on modest hardware. Override any time with `/model`, `AGENT_MODEL`,
or `modelOverride` in your config. Append `--default` to `/model` or `/backend`
to write the choice into `agent.config.json` (surgical merge: only `backend` and
`modelOverride`). `/model --default` pins the active model; `/backend --default`
pins the active backend and clears `modelOverride` so the next launch uses that
backend's built-in default model. In the interactive pickers you can also append
answer the `y/N` save prompt after choosing; each entry is tagged `(selected)`
for the live session choice and `(default)` for what's persisted on disk.

| Backend | Default model |
|---------|---------------|
| `ollama` | `qwen2.5-coder:7b` |
| `lm-studio` | `qwen2.5-coder-7b-instruct` |
| `mlx` | `mlx-community/Qwen2.5-Coder-7B-Instruct-4bit` |
| `llamacpp` | `gpt-3.5-turbo` |
| `openrouter` | `qwen/qwen-2.5-coder-32b-instruct` |
| `openai` | `gpt-4o-mini` |
| `openai-codex` | `gpt-5.5` |
| `grok` | `grok-4.5` |

### Recommend the right model for your box

All model-tuning lives under `/doctor`:

```
/doctor recommend       rank installed + default + cached models for your hardware
/doctor autotune apply  switch to the top-scoring cached local model
/doctor --deep          probe streaming, usage chunks, tool calls, fallbacks
/doctor bench           measure warmup, first-token, and total latency
/doctor models          show cached per-model capability + benchmark records
```

---

## Tools and commands

### Tools

| Class | Tools |
|-------|-------|
| Read | `file_read`, `grep`, `list_dir`, `glob`, `repo_search` |
| Mutate (approval-gated) | `file_write`, `file_edit`, `apply_patch`, `batch_edit`, `shell` |
| Workflow | `run_tests`, `ship_status`, `web_fetch`, `update_plan`, `task`, `critique` |
| MCP | anything an MCP server exposes, surfaced as `mcp__<server>__<tool>` |

The default `toolSelection: "auto"` keeps the full working pool available for
any real request (so "build me a site" writes files instead of dumping code
into the chat) and sends no tools only for plain greetings. `fixed` always
sends the pool. Set the pool with `/tools file_read,grep,list_dir`, or
persistently in `agent.config.json`.

### Approval policies

| Policy | Behavior |
|--------|----------|
| `always` (default) | Every mutating call prompts you, with a diff preview |
| `dangerous-only` | File mutations and shell commands not clearly recognized as non-destructive prompt. Shell also strips hydrated API-key env vars from the child process. |
| `never` | No prompts — use only when you trust the model |

At each prompt: `[y]es`, `[n]o`, `[a]lways for this tool`, or `[s]ession-allow
this exact call`. The session cache resets on `/new`.

### Slash commands

**Session and config**
```
/help                  list commands
/new                   start a fresh conversation
/setup                 rerun the setup wizard
/config                show resolved configuration
/session [title <…>]   show / rename the current session
/sessions              list saved sessions
/resume latest|<id>    resume a saved session
/export current|<id>   export transcript to markdown or json
/undo                  revert the last agent turn's file mutations
/path                  fork, switch, diff, pick, or drop parallel session paths
/paths                 list saved session paths
```

**Operator modes and workflow**
```
/mode explore|edit|ship|review   switch operator preset
/plan <intent>                   expand a short intent into a spec (.small-harness/spec.md)
/plan route <intent>             create a low/medium/high routed execution plan
/plan status                     show .small-harness/plan.json task status
/plan execute [--yolo] [--max N] run ready routed-plan tasks
/plan validate                   check the spec's Done Criteria against the working diff
/shipcheck                       summarize git + test readiness
/ship [--tests]                  preview last-mile ship readiness and commit message
/ship commit --all|--staged-only guarded local git commit with ship record
/ship push                       guarded git push, setting upstream when needed
/ship pr [--base main]           create a draft GitHub PR via gh, or print the command
/ship status                     summarize open PR checks and review state
/scorecard                       show global quality PRs shipped
/scorecard current               show tracked tokens on the current repo/branch
/scorecard prs [limit]            list recent closed PRs (numbered)
/scorecard pr <n>                 drill into PR quality, sessions, and trace audit
/scorecard verify <n>|--all       append GitHub PR checks/review/merge verification
/scorecard close <label> [--url <url>] [--tests]  close branch with shipcheck quality score
/scorecard doctor                inspect the local scorecard ledger for malformed JSONL
/scorecard export [path]          copy the raw scorecard ledger before repair or sharing
/fable                           show Claude Fable weekly usage and cap headroom
/handoff                         draft commit, changelog, release copy
/test discover|run|smart         discover or run tests
/fix                             fix-until-green loop
/iterate <goal>                  generate→evaluate→improve loop (rubric-scored)
/auto <goal> | --spec            autonomous overnight run (iterate + auto-reset, budget/deadline)
/batch / /refactor               coordinated multi-file edits
/play fix-failing-test           bundled demo in an isolated sandbox
```

**Backend, model, tools**
```
/backend <name> [--default]  switch backend; --default writes agent.config.json
/model [id] [--default]      list / pick a model; --default pins backend+model
/tools auto|fixed|<…>  show or set the active tool pool
/auth                  manage API keys and OAuth credentials
/login [provider]      sign in (defaults to active OAuth backend)
/logout [provider]     clear stored OAuth login (defaults to active OAuth backend)
/login openai-codex    sign in with ChatGPT/Codex subscription OAuth
/logout openai-codex   clear the stored ChatGPT/Codex login
/login grok            sign in with SuperGrok / X Premium+ OAuth
/logout grok           clear the stored Grok login
/image <path>          attach an image to the next user turn
/reasoning on|off      toggle the streaming reasoning panel
/verbose on|off        show every tool call with its full args + result
/trace on|off          show nested subagent/critic tool calls (indented)
/hooks                 list, trust, enable, or disable configured hooks
/compare [model]       re-send the last prompt against OpenRouter for A/B
/fusion on|tool|off    use OpenRouter Fusion alias or attach Fusion to a model
/route select|apply    select or apply a configured multi-model stack route
```

**Memory, capabilities, context**
```
/index                 build / refresh project memory
/map [query]           print a repo map or focused hits
/remember <text>       save a durable project note
/forget <id|all>       remove notes
/context               show prompt budget, model limit, auto-guard status
/compact               summarize older turns (auto-runs at threshold)
/reset                 write a continuation handoff and start a fresh session
/doctor [--deep]       probe backend, tools, streaming, capabilities
/doctor models         show cached per-model capability + benchmark records
/doctor autotune       pick the best cached local model (add `apply` to switch)
/doctor recommend      rank models for your hardware
/doctor bench          measure warmup + first-token + total latency
/checkpoints           toggle per-turn snapshots
```

Run `/help` in the harness for the full list with descriptions.

---

## Cost and credentials

### Credentials with `/auth` and `/login`

API-key cloud backends authenticate with API keys. Paste them once and Small
Harness stores them at `~/.config/small-harness/auth.json` (mode `0600`).
Environment variables always win at lookup time, so CI and scripted users see
no change in behavior.

```text
/auth                    show what's configured (keys are masked)
/auth set openai         paste your OpenAI key, save to file + this session
/auth set openrouter     paste your OpenRouter key
/auth clear openai       remove from the file (env stays for this session)
/login                   browser/device-code login for the active OAuth backend
/logout                  clear stored login for the active OAuth backend
/login openai-codex      browser/device-code login with ChatGPT/Codex
/logout openai-codex     remove the stored OAuth credential
/login grok              browser/device-code login with SuperGrok / X Premium+
/logout grok             remove the stored Grok OAuth credential
```

Bare `/login` and `/logout` use the **active OAuth backend** (`grok` or
`openai-codex`). On a non-OAuth backend (or via `/auth login` with no
provider), pass the provider explicitly.


`openai-codex` is not an `OPENAI_API_KEY` replacement. It uses browser/device
OAuth, stores `{access, refresh, expires, accountId}` in the same `auth.json`,
refreshes the access token before use, and sends model traffic to the Codex
Responses backend.

`grok` is not an `XAI_API_KEY` replacement either. It uses the same OAuth
shape as the official Grok CLI (browser PKCE on localhost, or RFC 8628
device-code for SSH/headless), stores tokens under the `grok` key in
`auth.json`, refreshes automatically, and calls xAI's Grok CLI inference proxy
with its required OAuth headers. If `~/.grok/auth.json` already has a Grok CLI
login, `/login grok` can import and refresh those credentials.

### Per-turn and session cost

When you're on a cloud backend with known pricing or provider-reported usage
cost, every turn prints its own cost plus the running session total:

```text
  2.1k in · 845 out · $0.013 this turn · $0.094 session
```

Switch to Ollama mid-session and the line shows `$0.00 this turn` but keeps
the running total honest. OpenRouter returns `usage.cost` for many requests,
including dynamic routers like Fusion; Small Harness uses that reported value
when present. If a cloud model does not expose cost, the turn shows `$?` and
prefixes the session total with `≥` to signal it is a lower bound, not a
fiction.

The `/model` picker first accepts an optional text filter, then shows the same
data in an arrow-key menu. It tags the live session choice `(selected)` and the
value persisted in `agent.config.json` `(default)`:

```text
  ▸ 1) gpt-4o-mini  (selected) (default)   128k ctx · $0.15/$0.60 per Mtoken
    2) gpt-4o                               128k ctx · $2.50/$10.00 per Mtoken
    3) o1-mini                              128k ctx · $3.00/$12.00 per Mtoken
    4) type a model id…
  ↑/↓ move · Enter select · 1-9 jump · q cancel
```

After an interactive `/model` or `/backend` choice, answer the `y/N` prompt to
save it as the project default. Direct forms such as `/model gpt-4o --default`
and `/backend ollama --default` still switch and persist in one command.

### Claude Fable tracker

`/fable` rolls up the local turn ledger into a weekly Claude Fable tracker:
Fable tokens, Fable turns, Fable's share of tracked Claude-family usage, and
remaining allowance when you configure a weekly plan budget. Fable turns also
append a compact weekly tracker to the status footer.

By default, Fable models are detected by model IDs containing `fable`, and the
cap share is `0.5` (50%). Add this to `agent.config.json` when you know the
weekly Claude-plan token budget you want Small Harness to monitor:

```json
{
  "fable": {
    "weeklyTokenBudget": 200000,
    "capShare": 0.5,
    "weekStartsOn": "monday"
  }
}
```

The tracker only sees Small Harness turns recorded in the local ledger. It
cannot see usage from the Claude app or other clients.

### Quality PR scorecard

`/scorecard` tracks whether Small Harness-assisted PRs are shipping with good
**local** quality evidence at close time — not post-merge CI on GitHub. Each
successful interactive turn still records input + output tokens under the current
repo and branch, but tokens are context rather than the score. `/ship pr` closes
that branch as a PR unit automatically and attaches a quality snapshot from local
ship readiness: blockers, warnings, whether tests passed, and whether the GitHub
PR command succeeded.

If you open a PR outside the built-in flow, run `/scorecard close <label>` to
close it with the same shipcheck-based score. Add `--url <github-pr-url>` when
you have a PR link and `--tests` to run tests before scoring.

The default view shows quality PR count, quality rate, average quality score,
clean ships, PRs needing follow-up, tokens per quality PR, the open branch
total, and a GitHub-style daily grid. `/scorecard prs` lists numbered recent
closes with session and ship-record hints; `/scorecard pr <n>` shows the full
audit captured at close time — quality rubric, per-session turn-trace summaries
(turns, steps, tool calls, timing), paths to session event logs, and explicit
reasons when a scored PR did not count as quality-shipped.

For PRs with GitHub URLs, `/scorecard verify <n>` refreshes the remote outcome
through `gh pr view`: PR state, review decision, mergeability, and check-rollup
status. `/scorecard verify --all` appends verification events for all recent
verifiable PRs. This does not rewrite the local close-time score; it adds later
remote evidence that `/scorecard pr <n>` renders next to the original audit.

After enough turns on a feature branch, the turn footer nudges you to close via
`/ship pr`. Audit snapshots come from local event logs at close time; export raw
traces with `/export <session> events`.

A PR counts as quality-shipped when its local score meets `scorecard.qualityThreshold`
(default 80), tests passed, readiness was not blocked, and either the PR
creation command succeeded or a PR URL was captured with `--url`. Configure via
`scorecard` in `agent.config.json` or disable with `scorecard.enabled: false`.
Data is stored locally under the Small Harness data directory; `/scorecard path`
prints the exact JSONL file. Use `/scorecard doctor` if the ledger looks wrong;
malformed JSONL lines are skipped rather than allowed to break the scorecard, and
`/scorecard export [path]` copies the raw ledger before manual repair. `/scorecard
reset --yes` now saves a timestamped backup before removing the active store.

**Note:** `/play score` shows playground fixture results — unrelated to this
global quality PR scorecard.

---

## Going further

### Plan a feature first

`/plan <intent>` expands a one- or two-sentence intent into an ambitious spec
— goal, user outcomes, scope, done criteria, open questions — and writes it to
`.small-harness/spec.md`. It deliberately stays at the level of *what* and
*why*, not implementation, so an early spec doesn't lock in the wrong details.
`/plan show` prints the saved spec; `--export <path>` writes elsewhere.

`/plan route <intent>` is the complexity-aware execution-layer path. It uses
`modelSystem.planner` when configured (falling back to selector, high/medium/low
orchestrator, then the active model), asks for a JSON task graph, assigns each
node to `modelSystem.coders.low|medium|high`, and writes
`.small-harness/plan.json`. Use `--planner high`, `--planner selector`, or
`--planner backend:model-id` to override the planner for one route:

```text
/plan route --planner high add OAuth login with refresh and tests
/plan status
/plan execute --max 2
```

`/plan execute` runs ready tasks sequentially, switching the active backend/model
per task and saving status after each node. `--yolo` auto-approves tools for
unattended local execution.

To mix subscription and API usage, put subscription-backed models on tiers where
Small Harness has a real login backend, such as `openai-codex` after
`/login openai-codex` or `grok` after `/login grok`, and keep usage-billed
automation on `openai`, `openrouter`, or local backends. For Claude/Fable subscriptions, track usage
with `/fable`; direct unattended execution should stay on an API-compatible
backend unless you add an explicit Claude CLI adapter.

`/plan validate` closes the loop: it reads the spec's **Done Criteria** and
checks each one against the current working-tree diff (the same done-check
`/auto` runs each round), printing a met/unmet checklist so you can ask "am I
actually done?" by hand. Like `/iterate`, it sends the diff to the model, so it
runs on a local backend unless you set `rubric.allowCloud`.

### Generate, evaluate, iterate

`/iterate <goal>` runs a generate→evaluate→improve loop. After each attempt a
**separate, read-only critic agent** (`critique`) scores the work 0–10 against
a weighted rubric and hands back actionable feedback; the loop repeats —
refining or pivoting — until the score clears the threshold or it runs out of
rounds (`--max N`, default 6, capped at 15; `--threshold X`). The harness, not
the model, computes the weighted total and pass/fail, so a critic that
over-rates can't wave weak work through.

The rubric defaults to quality / originality / craft / functionality and
penalizes generic "AI slop"; override it with a `.small-harness/rubric.md`
using `## Name (weight: N)` sections. Set `iterate.evaluatorModel` to grade
with a *different* model than the generator — the cleanest version of the
generator/evaluator split. Turn on `rubric.liveVerify` and the critic runs your
test suite (via a fixed-surface `verify` tool — no arbitrary shell) before
scoring functionality. The `critique` tool is also available on its own for a
one-off, independent grade.

Workspace context is never sent to a cloud backend for grading unless you set
`rubric.allowCloud`.

### Reset over compaction

On a long task, `/reset` writes a structured handoff artifact — done, in
progress, key decisions, next steps, key files — to
`.small-harness/continue.md`, then starts a **fresh session seeded with only
that artifact**. Unlike `/compact`, which summarizes in place, this is a clean
context window carrying just what's needed to continue, which holds coherence
better over long runs. `/reset --dry-run` writes the artifact without clearing;
cloud backends require `--cloud`, since drafting the note sends the
conversation to the model.

### Run it overnight with `/auto`

`/auto` is the unattended version of the loop above: it runs `/iterate`'s
generate→evaluate round repeatedly and, when the context window fills, **fires
`/reset` automatically** — drafting a handoff and continuing in a fresh session
— so a run can go for hours without blowing its budget. The goal and the latest
feedback carry across each reset.

```
/auto "add retry logic to web_fetch" --budget 2.00 --deadline 6h
/auto --spec --max 20 --yolo        # drive the spec.md from /plan to done
```

Give it an inline goal, or `--spec` to read the goal and **Done Criteria** from
`.small-harness/spec.md` (written by `/plan`). With criteria present, each round
also checks them against the working-tree diff, and "done" means the rubric
threshold *and* every criterion is met — a lightweight spec-validator folded in.

| Flag | Meaning |
|------|---------|
| `--spec` | Read goal + Done Criteria from `.small-harness/spec.md` |
| `--max N` | Round ceiling (default 12, hard cap 40) |
| `--threshold X` | Per-round rubric pass bar (default `rubric.passThreshold`) |
| `--budget $` | Stop after this much **generator** spend |
| `--deadline 6h` | Wall-clock cap (`h`/`m`/`s`) |
| `--reset-at 0.75` | Context-fill ratio that triggers an auto-reset (0.50–0.95) |
| `--yolo` | Auto-approve mutations for the whole run |
| `--cloud` | Allow sending workspace context to a cloud backend |

The run is **always finitely bounded** (a `--max` ceiling applies even with no
other flag) and stops early on a stall — no score gain and no diff change for
three rounds. However it ends — goal met, budget/deadline/rounds exhausted,
stall, error, or Ctrl-C — it leaves a morning report at
`.small-harness/auto-report.md` with the verdict, per-round scores, the Done
Criteria checklist, cost, elapsed time, and reset count. Same guards as
`/iterate`: it runs on a local backend unless you pass `--cloud`, needs
`rubric.enabled`, and won't run inside a `/play` session. Defaults live in the
`auto` config block. `/undo` reaches back only to the last reset boundary, so
keep `checkpoints.enabled` on for an unattended run.

### Project-specific system prompt

Drop a markdown file at `.small-harness/prompt.md` in your repo and Small
Harness prepends it to the system prompt every turn. Use it for project
conventions ("snake_case everywhere", "ship via `make release`", "never
edit `vendor/`"). Auto-truncated at 8 KB.

### MCP servers

Add an `mcpServers` block to `agent.config.json`:

```json
{
  "mcpServers": {
    "fs": {
      "command": "/usr/local/bin/some-mcp-server",
      "args": ["--root", "/tmp"],
      "env": { "TOKEN": "abc" }
    }
  }
}
```

Small Harness spawns each server at startup, lists its tools, and exposes
them through the same approval-gated tool layer with names like
`mcp__fs__read_file`. JSON-RPC over stdio; no extra dependencies.

### Hooks

Hooks let trusted local commands observe or influence harness events. They are
useful for terminal integrations, status tracking, policy checks, and progress
bridges for launchers, terminal orchestrators, and agent status dashboards.

Project hooks live in `agent.config.json`:

```json
{
  "hooks": {
    "PlanUpdated": [
      {
        "hooks": [
          { "type": "command", "command": "$HOME/bin/agent-plan-hook" }
        ]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "shell|file_write",
        "hooks": [
          {
            "type": "command",
            "command": "$HOME/bin/check-tool-policy",
            "timeoutSec": 5
          }
        ]
      }
    ]
  }
}
```

Command hooks receive a JSON payload on stdin and may print JSON on stdout:

```json
{ "decision": "block", "reason": "shell command not allowed" }
```

Supported decisions are `allow`, `deny`, `block`, and `stop`. Hooks can also
return `additionalContext`, `updatedInput`, or `feedback`. For `PreToolUse`,
`updatedInput` is honored only with `{"decision":"allow"}` and is discarded if
any hook blocks, denies, or stops. Exit code `2` maps to a blocking decision using
stderr as the reason. For `PreToolUse` and `PermissionRequest`, hook runner
failures such as timeouts, spawn/pipe failures, and shell infrastructure exits
`126`/`127` fail closed and block gated execution; ordinary nonzero exits still
warn unless the hook explicitly blocks. A pre-execution `stop` prevents other
pending tool calls in the same assistant step from running; a `PostToolUse` stop
applies after that tool has already run and stops the next model step.

Hook events include `SessionStart`, `UserPromptSubmit`, `PreToolUse`,
`PermissionRequest`, `PostToolUse`, `PreCompact`, `PostCompact`,
`PlanUpdated`, `SubagentStart`, `SubagentStop`, `Stop`, and `SessionEnd`.
Payloads include common fields such as `hook_event_name`, `session_id`,
`turn_id`, `cwd`, `workspace_root`, `transcript_path`, `events_path`, `backend`,
`model`, `approval_policy`, and `source`, plus event-specific fields like
`tool_name`, `tool_input`, `tool_response`, and `progress`. `source` is
`interactive`, `one-shot`, `auto`, `fix`, `iterate`, or `play` depending on
what started the turn.

Hook payload stdin is raw and unredacted so trusted hooks can make decisions on
the actual prompt/tool data; do not log it unless your hook performs its own
redaction. Hook child processes start with a cleared environment and receive the
minimal inherited shell environment (`PATH`, `HOME` or Windows home/system vars),
explicit parent process variables listed in `envVars`, literal values from
`env`, plus `SMALL_HARNESS_HOOK_EVENT`, `SMALL_HARNESS_SESSION_ID`,
`SMALL_HARNESS_TURN_ID`, `SMALL_HARNESS_TRANSCRIPT_PATH`, and
`SMALL_HARNESS_EVENTS_PATH`. Parent LLM provider credentials are not passed
through unless a hook explicitly names them in `envVars`.

Matchers are Codex-style: absent, empty, or `*` matches all; exact `|`
alternation matches tool/event names; other matchers are treated as full-match
regexes. Use `.*` when partial regex matching is intended. Invalid matcher
regexes are shown by `/hooks` and skipped. `UserPromptSubmit`, `PlanUpdated`,
`Stop`, and `SessionEnd` ignore matchers. The default hook timeout is 600
seconds for Codex parity; status/progress hooks should set a shorter
`timeoutSec` if a slow hook would make the turn feel stuck.

The `task` tool uses `SubagentStart` and `SubagentStop`; it does not also run
generic `PostToolUse` hooks. `Stop` hook `additionalContext` and `feedback` are
bounded, redacted, and added as context for the next turn.

`PermissionRequest` runs only when the harness would otherwise ask for approval.
Use `PreToolUse` for blanket policy gates that must also cover auto-approved or
read-only tools.

Project hooks are skipped until their current hash is trusted in user-owned
state. Project-controlled `hooks.state` entries are ignored for execution
safety. Manage trust with:

```text
/hooks                         list hooks and trust state
/hooks trust <key>             trust one hook hash in user state
/hooks trust-all               trust all new/modified hooks
/hooks disable <key>           disable one hook
/hooks enable <key>            enable one hook
```

Trust is stored in `$XDG_CONFIG_HOME/small-harness/hooks-state.json`, falling
back to `~/.config/small-harness/hooks-state.json`. Trusted hook successes are
quiet in the normal TUI; warnings, blocks, denies, stops, and feedback are
shown. The event log records hook start/end/decision records with redacted,
bounded stdout/stderr previews.

Launchers can inject ephemeral managed launch hooks without changing user
config:

```bash
SMALL_HARNESS_MANAGED_HOOKS_FILE="$TMPDIR/agent-status-hooks.json" small-harness
```

`SMALL_HARNESS_MANAGED_HOOKS_JSON` accepts the same document inline for small
launchers. Managed launch hooks are not cryptographic signatures or Codex
enterprise-managed hooks; they are process-local launcher-trusted commands for
wrappers that own the process invocation. Small Harness intentionally reads
these only from the real process environment, not repo `.env` files. This lets
integrations observe status without mutating the user's config.

Use `envVars` when a managed hook command needs launcher state:

```json
{
  "source": "terminal-orchestrator",
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "zentty ipc agent-event",
            "envVars": [
              "ZENTTY_INSTANCE_SOCKET",
              "ZENTTY_WORKLANE_ID",
              "ZENTTY_PANE_ID",
              "ZENTTY_PANE_TOKEN"
            ]
          }
        ]
      }
    ]
  }
}
```

### Image input

`/image <path>` attaches an image to your next prompt. Small Harness encodes
it as a `data:image/...;base64,...` URL and sends it as a multi-part user
message. The catalog tracks which models accept images; you get a warning
if your current model isn't vision-capable.

### Web fetch

`web_fetch` (off by default, approval-gated) lets the agent pull a URL,
strip HTML to text, and read the result. Useful for docs and RFCs the model
needs to consult mid-task. Enable per session with
`/tools auto file_read,grep,list_dir,web_fetch` or persistently in your
config.

### Project memory

`/index` builds a safe local repo map at `.sessions/project-memory/`. It
stores metadata only — paths, language, symbols, headings, capped keyword
terms — never file bodies. It honors `.gitignore` and skips `.git`,
`.sessions`, `target`, `node_modules`, binaries, oversized files, and
common secret/env files. `/map` prints a compact view; `/remember <text>`
saves a durable project note.

### Compare clouds with `/compare`

`/compare` re-sends your last prompt against any OpenRouter model so you can
A/B a local response against a frontier one without leaving the session.
Requires `OPENROUTER_API_KEY`.

### Use OpenRouter Fusion

Fusion is useful when a normal coding model is not enough: design reviews,
multi-file architecture tradeoffs, incident debugging, dependency choices, or
questions where a bad answer is more expensive than a few extra completions.

```text
/fusion on
```

Switches the active backend to OpenRouter and the model to `openrouter/fusion`.
Use it for deliberative turns, then run `/fusion off` to return to the normal
OpenRouter default model.

```text
/fusion tool anthropic/claude-sonnet-4.5
/fusion tool anthropic/claude-sonnet-4.5 panel=~openai/gpt-latest,deepseek/deepseek-v3.2 judge=~anthropic/claude-opus-latest max-tools=4
```

Tool mode keeps a chosen OpenRouter coding model as the outer agent and adds
OpenRouter's Fusion plugin so the model can invoke multi-model deliberation
when the turn warrants it. The same Small Harness tools, approvals, session log,
token counts, and reported OpenRouter costs stay visible.

### Route tasks across a model system

`/route` lets you describe a model stack that blends local and frontier models:
separate orchestrators for low/medium/high planning, coders for
low/medium/high implementation, play and production review models, a security
review model, a compaction model that summarizes the transcript when context is
compacted, and one selector model that chooses the route for a task. `/route
status` shows the configured stack (including the compaction model) and `/route
template` prints the JSON shape to paste into `agent.config.json`.

```text
/route template
/route status
/route select add OAuth login with token refresh and tests
/route select --dry-run redesign the settings page
/route apply coder high
/route apply review production
/route apply security
```

`/route select` sends the task plus the configured stack to
`modelSystem.selector`, expects a JSON decision, prints the selected
orchestrator/coder/reviewer/security path, and switches the live session to the
chosen coding model unless `--dry-run` is passed. The selector can also return
`coderEffort`, `reviewEffort`, and `securityEffort` (`none`, `minimal`, `low`,
`medium`, `high`, `xhigh`, or `max`). The chosen coder effort becomes the
active session effort, appears in `/session` and the turn footer, and is sent to
OpenRouter as `reasoning.effort`; local backends ignore unsupported request
fields while still showing the selected effort.

For whole-goal decomposition, `/plan route <goal>` uses `modelSystem.planner`
or a planner override to create `.small-harness/plan.json`; `/plan execute`
then runs each ready node with the configured low/medium/high coder model.

Context compaction (summarizing the conversation when the prompt budget fills,
both automatically and via `/compact`) uses the main conversation model by
default. Set `modelSystem.compaction` to a `ModelRef` object (a `{ "backend":
"...", "model": "..." }` pair, the same shape as `planner`/`selector`) to
summarize with a different model, for example a cheaper or longer-context one:

```json
{
  "modelSystem": {
    "compaction": { "backend": "openrouter", "model": "anthropic/claude-3.5-haiku" }
  }
}
```

Prompt-budget sizing still tracks the main model (that is the context being
fit); only the summary call uses the compaction model. If the compaction
backend is not ready (for example a missing API key), compaction falls back to
the main model and prints a warning.

---

## Configuration

Resolution order (later overrides earlier):

1. Built-in defaults
2. `agent.config.json` in the working directory
3. `.env`, then `.env.local`
4. Process environment variables
5. Slash command overrides at runtime

### Environment variables (the useful ones)

```bash
BACKEND=ollama                                          # ollama|lm-studio|mlx|llamacpp|openrouter|openai|openai-codex|grok
AGENT_MODEL=qwen2.5-coder:14b                           # overrides the backend default model

OPENAI_API_KEY=sk-...                                   # required for openai
OPENROUTER_API_KEY=sk-or-...                            # required for openrouter / /compare
OPENAI_BASE_URL=https://api.openai.com/v1               # point at a compatible proxy if needed
OPENAI_CODEX_BASE_URL=https://chatgpt.com/backend-api    # override Codex backend base if needed

APPROVAL_POLICY=always                                  # always | dangerous-only | never
AGENT_TOOLS=file_read,grep,list_dir,file_edit,file_write,shell,update_plan,task
AGENT_TOOL_SELECTION=auto                               # auto | fixed

WARMUP=true                                             # pre-warm prompt cache at startup
SMALL_HARNESS_NO_WIZARD=false                           # skip first-run setup
SMALL_HARNESS_NO_UPDATE_CHECK=false                     # skip the GitHub release check
SMALL_HARNESS_MANAGED_HOOKS_JSON='{"source":"terminal-orchestrator","hooks":{...}}'
SMALL_HARNESS_MANAGED_HOOKS_FILE=/tmp/agent-status-hooks.json
```

Full list with comments in [`.env.example`](.env.example).

### `agent.config.json`

For project-level defaults, run `/setup`, use `/backend --default` /
`/model --default`, or drop a JSON file at the repo root. Common shape:

```json
{
  "backend": "ollama",
  "modelOverride": "qwen2.5-coder:14b",
  "approvalPolicy": "dangerous-only",
  "tools": ["file_read", "grep", "list_dir", "file_edit", "file_write", "shell", "update_plan", "task"],
  "toolSelection": "auto",
  "maxSteps": 20,
  "display": {
    "toolDisplay": "grouped",
    "eventLog": { "enabled": true }
  },
  "scorecard": {
    "enabled": true,
    "qualityThreshold": 80,
    "nudgeMinTurns": 3
  },
  "fable": {
    "enabled": true,
    "weeklyTokenBudget": null,
    "capShare": 0.5,
    "weekStartsOn": "monday"
  },
  "workspaceRoot": "/path/to/project",
  "outsideWorkspace": "prompt",
  "context": {
    "maxMessages": 40,
    "modelContextTokens": 8192,
    "autoCompact": true,
    "compactThreshold": 0.85,
    "reserveRatio": 0.25
  },
  "projectMemory": {
    "enabled": true,
    "autoInject": true,
    "allowCloudContext": false
  },
  "checkpoints": { "enabled": true, "maxTurns": 10 },
  "rubric": { "enabled": true, "passThreshold": 7.0, "allowCloud": false, "liveVerify": false },
  "iterate": { "maxIters": 6, "evaluatorModel": null },
  "auto": { "maxRounds": 12, "budgetUsd": null, "resetRatio": 0.75, "deadline": null },
  "paths": {
    "enabled": true,
    "maxPaths": 5,
    "maxSnapshotBytes": 52428800,
    "maxFileBytes": 1048576
  },
  "openrouter": {
    "fusion": {
      "enabled": false,
      "analysisModels": [],
      "judgeModel": null,
      "maxToolCalls": null
    }
  },
  "modelSystem": {
    "enabled": true,
    "planner": {
      "backend": "openrouter",
      "model": "anthropic/claude-opus-4.8",
      "effort": "high",
      "thinkingDepth": "deep"
    },
    "selector": {
      "backend": "openrouter",
      "model": "openrouter/fusion",
      "effort": "high",
      "thinkingDepth": "deep"
    },
    "orchestrators": {
      "low": { "backend": "ollama", "model": "qwen2.5-coder:7b" },
      "medium": { "backend": "openrouter", "model": "qwen/qwen-2.5-coder-32b-instruct" },
      "high": { "backend": "openrouter", "model": "anthropic/claude-sonnet-4.5" }
    },
    "coders": {
      "low": { "backend": "ollama", "model": "qwen2.5-coder:7b" },
      "medium": {
        "backend": "openrouter",
        "model": "qwen/qwen-2.5-coder-32b-instruct",
        "effort": "medium"
      },
      "high": {
        "backend": "openrouter",
        "model": "anthropic/claude-sonnet-4.5",
        "effort": "high"
      }
    },
    "reviewers": {
      "play": { "backend": "ollama", "model": "qwen2.5-coder:7b" },
      "production": { "backend": "openrouter", "model": "openrouter/fusion" }
    },
    "securityReviewer": { "backend": "openrouter", "model": "openrouter/fusion" }
  },
  "mcpServers": {
    "fs": { "command": "/usr/local/bin/some-mcp-server", "args": [] }
  },
  "hooks": {
    "PlanUpdated": [
      { "hooks": [{ "type": "command", "command": "$HOME/bin/plan-hook" }] }
    ]
  }
}
```

Anything in the config can be overridden by env or slash commands at
runtime.

---

## Quality of life

- **`small-harness --continue`** resumes the most recent session in cwd
  without picking from a list.
- **`small-harness completions bash|zsh|fish`** prints a completion script
  you can source.
- **`/reasoning on|off`** toggles the streaming reasoning panel — adds a
  dim "thinking…" block above the answer for o-series and similar models.
- **`/verbose on|off`** switches to a debug tool view: every tool call is
  printed with its full arguments and a large result preview, so you can see
  exactly what the agent is doing. `/verbose off` restores the normal view.
- **`/trace on|off`** shows nested subagent and critic tool activity as
  indented lines in the TUI (without flooding the parent context). Every turn
  is also logged to a sidecar at `.sessions/<session-id>.events.jsonl` with
  tool calls, approvals, compaction, warmup, and timing — enabled by default
  via `display.eventLog.enabled` in `agent.config.json`.
- **Turn footer timing.** After each turn the status line includes step count
  and a breakdown when available: `TTFT`, `model`, `tools`, `approval`, and
  `total` seconds alongside the existing token and cost stats.
- **Slash-command completion.** Type `/` and a menu of matching commands (with
  descriptions) appears beneath the prompt; the best match also shows as dim
  ghost text. **↑/↓** select, **Tab** accepts (with a trailing space), **→**
  accepts inline, **Esc** dismisses. It narrows live as you type.
- **Update check.** Once a day, Small Harness checks GitHub for a newer
  release and shows a one-line notice in the banner if there is one.
  Background, cached, opt-out with `SMALL_HARNESS_NO_UPDATE_CHECK=true`.
- **Crash log.** If the harness panics, it writes a redacted log (API keys
  scrubbed) to `.sessions/crashes/<timestamp>.log` and prints the path so
  you have something to attach to an issue.
- **One-shot mode** — `small-harness --print "summarize this repo"` or
  `printf '…\n' | small-harness` for scripts and CI. Approval-gated tools
  are denied by default; pass `--allow-tools` to allow them.
- **Agent eval** — `small-harness --eval fix-failing-test [--model M] [--json]`
  runs a bundled eval fixture and exits 0/1 (for CI scripts). `--eval` can
  also point at a data-only fixture JSON file; its workspace is resolved
  relative to that file and rejected if it escapes the fixture root. In the
  interactive TUI, `/eval agent <fixture.json>` accepts the same external
  fixture path.
- **Warmup.** Small Harness sends a 1-token request with the full system
  prompt + tools at startup so llama.cpp-derived engines have a hot
  prompt-eval cache before your first prompt. Disable with `WARMUP=false`.

---

## Troubleshooting

### `Backend not reachable: Connection error`

- **Ollama** — `brew services start ollama` or run `ollama serve`. Default port 11434.
- **LM Studio** — open the app, go to Local Server, click Start. Default port 1234.
- **MLX** — start `mlx_lm.server --port 8080` against an MLX-format model.
- **llama.cpp** — `llama-server -m /path/to/model.gguf --host 127.0.0.1 --port 8080 --jinja` (the `--jinja` flag enables native tool calls).
- **OpenRouter** — set `OPENROUTER_API_KEY` (or use `/auth set openrouter`).
- **OpenAI** — set `OPENAI_API_KEY` (or use `/auth set openai`). Use `OPENAI_BASE_URL` for a compatible proxy.
- **OpenAI Codex** — run `/login openai-codex`, then `/backend openai-codex`.
- **Grok** — run `/login grok` (browser or device-code), then `/backend grok`.

Run `/doctor --deep` for a fuller capability probe (streaming, usage chunks,
native tool calls, inline JSON fallback). Reports land under `.sessions/doctor/`.

### First prompt is slow even with warmup

The cache becomes stale when you change `/backend`, `/model`, or `/tools`.
The next prompt re-evaluates the new system prompt and tools. One-time per
change.

### Model returns tool calls as text JSON

Some small-model templates emit tool calls as plain content
(`{"name": "shell", "arguments": {…}}`) instead of populating the
`tool_calls` field. Small Harness detects and synthesizes a real tool call.
If a particular model still misbehaves, `llama3.1:8b` has well-tested
tool-call templates.

### Model responds in another language unexpectedly

Some bilingual models (notably qwen) drift into Chinese on short greetings.
The system prompt has an explicit language directive; if it's still
happening, strengthen it by editing `SYSTEM_PROMPT` in `src/config.rs`.

### `cargo: command not found`

Install Rust via [rustup](https://rustup.rs):
`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`.

---

## Architecture

```text
                +-------------------------+
                |        main.rs          |
                |  banner / input loop /  |
                |  warmup / approval      |
                +------------+------------+
                             |
                             v
+-----------+    +-------------------------+    +-------------------+
| config.rs |--->|        agent.rs         |<-->|    tools/*.rs     |
|  + auth/  |    |  chat/completions loop  |    | + mcp__ adapters  |
+-----------+    +-------------+-----------+    +-------------------+
                               |
                               v
                +-------------------------+
                |       backends.rs       |
                |  Ollama / LM Studio /   |
                |  MLX / llama.cpp /      |
                |  OpenRouter / OpenAI    |
                +-------------------------+
```

Source layout in [`src/`](src/) — `agent.rs` runs the loop, `backends.rs`
holds the backend providers, `tools/` holds tool implementations, `mcp.rs` is
the stdio MCP client, `catalog.rs` has the per-model context + pricing
table, `auth.rs` manages the credential file, `session.rs` writes the JSONL
log. `cargo doc --open` for module-level docs.

---

## Contributing

```bash
cargo check                # type-check without producing a binary
cargo run --release        # optimized build + run
cargo build --release      # target/release/small-harness
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Guidelines:

- Mutating tools implement `require_approval` on the `Tool` trait (return
  `true`, or compute from args — see `shell.rs`).
- New backends usually need an OpenAI-compatible `/v1/chat/completions`
  endpoint and a default model in `backends.rs`; non-compatible transports
  should add an adapter like `codex_responses.rs`.
- Before opening a PR, run the full check suite: `cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo test`.

Release tags use a leading `v` (`v0.4.0`). The release workflow at
[`.github/workflows/release.yml`](.github/workflows/release.yml) builds
notarized macOS binaries when Apple Developer secrets are present.

---

## License

[MIT](LICENSE).
