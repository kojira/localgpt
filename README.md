
# LocalGPT

A local device focused AI assistant built in Rust — persistent memory, autonomous tasks, ~27MB binary. Inspired by and compatible with OpenClaw.

`cargo install localgpt`

## Why LocalGPT?

- **Single binary** — no Node.js, Docker, or Python required
- **Local device focused** — runs entirely on your machine, your memory data stays yours
- **Persistent memory** — markdown-based knowledge store with full-text and semantic search
- **Autonomous heartbeat** — delegate tasks and let it work in the background
- **Multiple interfaces** — CLI, web UI, desktop GUI
- **Multiple LLM providers** — Anthropic (Claude), OpenAI, Ollama
- **Plugin Tags** — config-driven tag system for executing external commands from LLM responses
- **OpenClaw compatible** — works with SOUL, MEMORY, HEARTBEAT markdown files and skills format

## Install

```bash
# Full install (includes desktop GUI)
cargo install localgpt

# Headless (no desktop GUI — for servers, Docker, CI)
cargo install localgpt --no-default-features
```

## Quick Start

```bash
# Initialize configuration
localgpt config init

# Start interactive chat
localgpt chat

# Ask a single question
localgpt ask "What is the meaning of life?"

# Run as a daemon with heartbeat, HTTP API and web ui
localgpt daemon start
```

## How It Works

LocalGPT uses plain markdown files as its memory:

```
~/.localgpt/workspace/
├── MEMORY.md            # Long-term knowledge (auto-loaded each session)
├── HEARTBEAT.md         # Autonomous task queue
├── SOUL.md              # Personality and behavioral guidance
└── knowledge/           # Structured knowledge bank (optional)
    ├── finance/
    ├── legal/
    └── tech/
```

Files are indexed with SQLite FTS5 for fast keyword search, and sqlite-vec for semantic search with local embeddings 

## Configuration

Stored at `~/.localgpt/config.toml`:

```toml
[agent]
default_model = "claude-cli/opus"

[providers.anthropic]
api_key = "${ANTHROPIC_API_KEY}"

[heartbeat]
enabled = true
interval = "30m"
active_hours = { start = "09:00", end = "22:00" }

[memory]
workspace = "~/.localgpt/workspace"
```

## CLI Commands

```bash
# Chat
localgpt chat                     # Interactive chat
localgpt chat --session <id>      # Resume session
localgpt ask "question"           # Single question

# Daemon
localgpt daemon start             # Start background daemon
localgpt daemon stop              # Stop daemon
localgpt daemon status            # Show status
localgpt daemon heartbeat         # Run one heartbeat cycle

# Memory
localgpt memory search "query"    # Search memory
localgpt memory reindex           # Reindex files
localgpt memory stats             # Show statistics

# Config
localgpt config init              # Create default config
localgpt config show              # Show current config
```

## HTTP API

When the daemon is running:

| Endpoint | Description |
|----------|-------------|
| `GET /health` | Health check |
| `GET /api/status` | Server status |
| `POST /api/chat` | Chat with the assistant |
| `GET /api/memory/search?q=<query>` | Search memory |
| `GET /api/memory/stats` | Memory statistics |

## Blog

[Why I Built LocalGPT in 4 Nights](https://localgpt.app/blog/why-i-built-localgpt-in-4-nights) — the full story with commit-by-commit breakdown.

## Built With

Rust, Tokio, Axum, SQLite (FTS5 + sqlite-vec), fastembed, eframe

## Contributors

<a href="https://github.com/localgpt-app/localgpt/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=localgpt-app/localgpt" />
</a>

## Stargazers

[![Star History Chart](https://api.star-history.com/svg?repos=localgpt-app/localgpt&type=Date)](https://star-history.com/#localgpt-app/localgpt&Date)

## Plugin Tags（プラグインタグシステム）

LLMの応答テキストにタグを埋め込むだけで外部コマンドを実行できる、config駆動の軽量プラグインシステム。

### 特徴

- タグ名・コマンドパターン・テンプレートすべてconfig.tomlで定義
- Rustコード変更不要で新しいタグを追加可能
- ツールコール不要 → トークン消費を大幅削減
- オプショナルなconfig差し替え機能（複数アカウント切替等に対応）

### 仕組み

1. LLMが応答に `[TAG_NAME:引数1:引数2...]` を含める
2. LocalGPTが後処理でタグを検出・パース
3. config.tomlのテンプレートに基づいてコマンド生成・実行
4. タグはDiscord送信前にテキストから除去される

### Configuration

Add to `~/.localgpt/config.toml`:

```toml
[tags.nostaro]
binary = "/path/to/nostaro"
config_swap = "~/.nostaro-howari"  # Optional: swap config before execution

[tags.nostaro.patterns]
"post:{message}" = "{binary} post \"{message}\""
"reply:{note_id}:{message}" = "{binary} reply {note_id} \"{message}\""
"react:{note_id}:{emoji}" = "{binary} react {note_id} --content \"{emoji}\""
"channel:post:{channel_id}:{message}" = "{binary} channel post {channel_id} \"{message}\""
"channel:create:{name}:{about}" = "{binary} channel create --name \"{name}\" --about \"{about}\""

[tags.cmd]
[tags.cmd.patterns]
"ls:{path}" = "ls {path}"
"echo:{message}" = "echo \"{message}\""
```

### Use Cases

**Nostr posting** — Agent posts to Nostr while chatting on Discord:
- `[NOSTARO:post:おはよう☁️]` → Post to Nostr via nostaro
- `[NOSTARO:react:note1xxx:⚡]` → React on Nostr

**Shell commands** — Execute system commands alongside responses:
- `[CMD:echo:ログメッセージ]` → Run echo command
- `[CMD:ls:/home]` → List directory

**Multi-account** — `config_swap` auto-switches between different account configs

**Weather** —
```toml
[tags.weather]
[tags.weather.patterns]
"{city}" = "curl -s wttr.in/{city}?format=3"
```
`[WEATHER:Tokyo]` → Fetch weather info

**Token savings** — No round-trip needed unlike tool calls:
- Tool call: prompt → tool decision → execute → result → final answer (2x input tokens)
- Tag system: prompt → answer+tag → post-process (1x input tokens)

### Template Syntax

- `{binary}` → Replaced with TagGroup's binary field
- `{placeholder}` → Replaced with pattern-matched value
- Last placeholder captures all remaining segments (supports strings with colons)

### Security Notes

- Only specify trusted directories for `config_swap`
- Only define safe commands in `patterns` — LLM can trigger any defined pattern

## Discord Integration

LocalGPT can connect to Discord as a bot, responding to messages in specified channels.

### Configuration

Add to `~/.localgpt/config.toml`:

```toml
[channels.discord]
enabled = true
token = "${DISCORD_BOT_TOKEN}"
allow_bots = false

[[channels.discord.guilds]]
guild_id = "123456789012345678"
channels = ["987654321098765432"]  # Empty = all channels
require_mention = false            # true = only respond when @mentioned
```

Start the daemon to activate:

```bash
localgpt daemon start
```

### Known Issues & Workarounds

| Issue | Cause | Workaround |
|-------|-------|------------|
| CLAUDE.md interference | Claude CLI loads project CLAUDE.md, conflicting with SOUL.md persona | `--setting-sources user` flag (already applied) |
| Session history pollution | Stale session history overrides SOUL.md persona changes | Automatic session cleanup on SOUL.md change detection |
| SOUL.md dynamic reload | Persona changes require session restart | Auto-detected via file modification time; triggers session reset |

### Batch Message Processing

When multiple Discord messages arrive in quick succession, LocalGPT batches them together before sending to the LLM. This reduces API calls and provides better context for responses.

- Messages within the batch window are combined into a single prompt
- Each message includes the sender's username for context
- Per-channel sessions maintain conversation continuity

## License

[Apache-2.0](LICENSE)
