# README media

| File | What |
|------|------|
| `agent-session.gif` | Looping TUI session (1s hold on the final frame) |
| `agent-session.mp4` | Same session as video |

Recorded with [VHS](https://github.com/charmbracelet/vhs) against local Ollama
`qwen2.5:32b`. Regenerate from the repo root with `albatross` on your `PATH`
and a small demo workspace at `./demo-sandbox`:

```bash
vhs docs/tapes/agent-session.tape
```
