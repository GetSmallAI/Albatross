# Dense Albatross TUI session (config-only)
# Requires: albatross on PATH, Ollama with qwen2.5:32b, and a small demo
# workspace at ./demo-sandbox (agent.config.json + sample project files).
Output "docs/assets/demo/agent-session.raw.gif"
Output "docs/assets/demo/agent-session.raw.mp4"

Set Shell "bash"
Set FontSize 14
Set FontFamily "JetBrains Mono"
Set Width 1080
Set Height 620
Set Padding 8
Set Theme "Catppuccin Mocha"
Set TypingSpeed 12ms
Set Framerate 30
Set PlaybackSpeed 1.4
Set LoopOffset 0%
Set CursorBlink false
Set WindowBar Colorful
Set WindowBarSize 28
Set BorderRadius 6

Hide
Type "cd ./demo-sandbox && clear"
Enter
Sleep 100ms
Show

Type "albatross"
Enter
Sleep 2s

Type "List project files, run python3 snake_lite.py --demo, quote the score line"
Enter
Sleep 18s

Type "Run tests/test_snake.py with pytest or a plain python assert on play demo"
Enter
Sleep 16s

Type "Edit snake_lite.py so demo mode prints ALBATROSS SNAKE first, then re-run the demo"
Enter
Sleep 18s

Type "Show the first 15 lines of snake_lite.py"
Enter
Sleep 12s

Type "/help"
Enter
Sleep 1.4s

Type "/provider"
Enter
Sleep 1.4s
Type "q"
Sleep 200ms

Type "/play"
Enter
Sleep 1.2s

Sleep 1.4s
Type "Ctrl+C"
Sleep 120ms
