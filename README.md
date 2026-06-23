# shellmind (`sm`)

A cross-platform personal automation kernel with semantic memory. `sm` sits in front of your shell as a resolution layer — translating natural and custom command vocabulary into real commands, and learning from use.

It is **not** a smarter shell. It is a layer that resolves what you mean into what your shell should run.

---

## How it works

When you type `sm <anything>`, the shell hook captures it and passes it through a 4-stage resolution pipeline:

1. **Exact match** — looks up your verb directly in the registry (curated-first)
2. **Prefix / fuzzy match** — starts-with, ends-with, or Levenshtein distance ≤ 2
3. **Semantic similarity** — embedding cosine search (≥ 0.72); BoW hash by default, MiniLM-L6-v2 via the optional `semantic-embeddings` feature
4. **Model inference** — calls an OpenAI-compatible LLM to infer intent; result is stored in staging for review

When multiple candidates are close, a disambiguation prompt is shown on stderr. When the winner is clear (score gap ≥ 0.12 or score ≥ 0.94), it auto-selects silently.

---

## Features

- **Alias registry** — store named one-liner expansions with positional or named argument slots (`$1`, `{label}`)
- **Procedures** — multi-step workflows built from typed, cross-platform `StepKind`s (file ops, HTTP calls, env vars, shell raw, etc.)
- **Session wrapping** — `sm wrap last <N> as <verb>` captures recent successful commands into a reusable procedure
- **Semantic memory** — flat binary embedding store; verbs are embedded on write and searched at resolve time
- **Two-tier registry** — curated (trusted) and staging (inferred/unconfirmed); automatic promotion after 5 uses with confidence ≥ 0.70
- **Confidence decay** — staging entries not used in 30 days are flagged for review
- **Shell-agnostic hook** — works with Bash, Zsh, Fish, PowerShell, and Cmd; `sm init [--write]` prints the correct snippet for your shell or appends it to your rc file directly. Bash/Zsh/Fish/PowerShell all capture native commands via `PROMPT_COMMAND`/`precmd`/`fish_postexec`/prompt-wrapping so `sm wrap` can see them
- **Scheduled procedures** — `sm schedule add <name> "<cron>" <verb> [args...]` registers a 5-field cron; `sm schedule run` executes all due jobs (wire it into Unix cron or Windows Task Scheduler)
- **Model-free by default** — all four resolution stages degrade gracefully; model inference is opt-in via environment variable
- **Cross-platform executor** — 12 `StepKind` variants implemented in pure Rust with no `awk`/`python`/shell dependencies

---

## Commands

| Command | Description |
|---|---|
| `sm init` | Print the shell hook snippet to paste into your rc file |
| `sm init --write` | Detect your rc file and append the hook (idempotent) |
| `sm hook` | Print the hook snippet only |
| `sm add <verb> "<expansion>" [args...]` | Add a one-liner alias immediately to curated |
| `sm add <verb>` | Interactive alias wizard (offers `[e]dit` on collision) |
| `sm add proc <verb>` | Interactive procedure builder (12 step types) |
| `sm wrap last <N> as <verb>` | Wrap the last N successful commands into a procedure |
| `sm resolve <input>` | Resolve input and print the shell command (called by the hook) |
| `sm run <verb> [args...]` | Execute a verb directly, bypassing the shell hook |
| `sm edit <verb>` | Open the verb's definition in `$EDITOR`, re-index on save |
| `sm rename <old> <new>` | Rename a verb, preserving its definition, embedding, and history |
| `sm remove <verb>` | Delete a verb from the registry and embedding store |
| `sm confirm <verb>` | Promote a staging entry to curated |
| `sm demote <verb>` | Move a curated entry back to staging |
| `sm list [--detail] [--by-usage|--by-date] [pattern]` | List curated entries, with optional filter and sort |
| `sm list --staging` | List staging entries |
| `sm reindex` | Rebuild the redb cache and embedding store from TOML registry |
| `sm promote` | Run the promotion pass (decay, auto-promote, flag for review) |
| `sm schedule add <name> "<cron>" <verb> [args...]` | Schedule a verb on a 5-field cron expression |
| `sm schedule list` | List configured schedules with next-run times |
| `sm schedule next` | Show upcoming runs sorted ascending |
| `sm schedule run` | Run all due schedules now (wire into cron/Task Scheduler) |
| `sm schedule remove <name>` / `enable <name>` / `disable <name>` | Lifecycle management |

---

## Setup

### Prerequisites

- Rust ≥ 1.80 (required by `redb` and `fastembed`; tested on 1.89)
- A supported shell: Bash, Zsh, Fish, PowerShell, or Cmd

### Quick start (recommended)

```bash
# Build with the full recommended feature set
# (~80MB MiniLM ONNX download on first run, ~2min cold compile)
cargo build --release --no-default-features --features semantic-embeddings

# Install on $PATH
cp target/release/sm ~/.local/bin/             # Linux / Mac
# Windows: copy target\release\sm.exe to any directory on PATH

# Auto-detect your shell rc file and append the hook
sm init --write
exec $SHELL                                    # reload the shell

# Optional but recommended: enable LLM-backed Stage-4 fallback
export OPENAI_API_KEY=sk-...
```

That gets you the full pipeline: MiniLM-L6-v2 embeddings for semantic
search, redb O(1) verb lookup, native command capture for `sm wrap`, and
LLM inference for unknown inputs. The MiniLM ONNX model is downloaded
into the platform cache dir on the first semantic-embedding call.

### Build options

| Build command | Embeddings | First-run download | Cold compile | When to use |
|---|---|---|---|---|
| `cargo build --release --no-default-features --features semantic-embeddings` | **MiniLM-L6-v2** (384-dim) | ~80MB ONNX | ~2 min | **Recommended** — production-quality Stage-3 semantic similarity |
| `cargo build --release` | BoW hash (512-dim) | none | ~30s | Quick iteration, CI, sandboxes, exact-match-heavy use |

Both modes always include:

- **`redb`** — O(1) verb lookup cache for Stage-1 / `sm list`
- **Shell hook** for bash/zsh/fish/PowerShell with native command capture
- **Scheduler** (`sm schedule …`) — cron-driven verb execution
- **All 12 `StepKind` executors** for typed procedures

Switching embedding modes requires `sm reindex` to rebuild `embeddings.bin`
at the new vector dimension. The `--release` flag matters a lot for the
semantic build — debug ONNX is unusably slow.

### Wire up the shell hook

```bash
sm init           # prints the snippet for review
sm init --write   # auto-detects your rc file and appends (idempotent)
```

`sm init` auto-detects bash/zsh/fish on Unix from `$SHELL`, and PowerShell
on Windows via `$PROFILE` (preferring PowerShell 7+ over Windows
PowerShell 5.x when both are present). Re-running `--write` on an
already-installed rc file reports "Hook already installed" and exits
cleanly.

The installed hook also registers a `__record` capture via your shell's
prompt mechanism (`PROMPT_COMMAND` on bash, `precmd_functions` on zsh,
`fish_postexec` on fish, prompt-function wrapping on PowerShell) so
`sm wrap last <N>` can see native commands that didn't go through `sm`.

### Optional: model inference (Stage 4)

Set `OPENAI_API_KEY` (or equivalent for any OpenAI-compatible endpoint).
The default model is `gpt-4o-mini`. Without a key, Stage 4 passes through
unchanged — your shell sees the input verbatim.

For a non-OpenAI endpoint, edit `~/.config/shellmind/shellmind.toml`:

```toml
model_name  = "your-model"
api_base    = "https://your-endpoint/v1"
api_key_env = "YOUR_API_KEY_VAR"
```

### Optional: schedule the cron tick

For `sm schedule run` to actually fire due jobs, wire it into your OS
scheduler:

```bash
# Unix cron — every minute:
(crontab -l 2>/dev/null; echo "* * * * * $HOME/.local/bin/sm schedule run >/dev/null 2>&1") | crontab -
```

```powershell
# Windows Task Scheduler:
schtasks /create /sc minute /mo 1 /tn shellmind-tick /tr "C:\path\to\sm.exe schedule run"
```

### Logging

Set `SHELLMIND_LOG=debug` (or `info`, `warn`) to enable tracing output to
stderr.

---

## Registry file format

Entries live in `~/.config/shellmind/registry/curated/` and `.../staging/` (Linux/Mac) or `%APPDATA%\shellmind\registry\` (Windows). Files are named `<verb>.toml` and contain JSON.

**Alias:**
```json
{
  "id": "uuid-v4",
  "verb": "post_ingest",
  "confidence": 1.0,
  "source": "declarative",
  "created_at": "2026-05-10T00:00:00Z",
  "usage_count": 3,
  "tags": [],
  "expansion": "curl -s -X POST {url} -H 'Content-Type: text/csv' --data-binary @{file}",
  "arg_names": ["url", "file"]
}
```

Source trust levels: `declarative` → `inferred:<model>:<score>` → `confirmed:<score>` → `wrapped`.

---

## TODOs / Roadmap

Completed: `sm init --write`, `redb` cache, optional `fastembed` (MiniLM-L6-v2) embeddings, `sm edit`, `sm remove`, `sm rename`, `sm run`, shell-hook `__record` capture across bash/zsh/fish/PowerShell, native `StepKind` inference in `sm wrap` (curl/wget/cp/mv/rm/mkdir/echo/export/env-prefix), Windows hook hardening (pwsh.exe detection, PS prompt-wrap `__record`, full mgmt list), scheduled procedures via `sm schedule`.

| Priority | Feature | Notes |
|---|---|---|
| 1 | Config file parser | Replace line-by-line model config parsing with `serde_json` / `toml` |
| 2 | Drop the rest of the `=` version pins | Inherited from a Rust 1.75 sandbox era; modern toolchain doesn't need them |
| 3 | Real HTTP for `HttpCall` | Currently stubbed — needs `reqwest` (was deliberately deferred from the original sandbox cutover) |
| 4 | Windows VT escape enablement | ANSI colour codes render as literal text in legacy `conhost.exe`; need `SetConsoleMode(ENABLE_VIRTUAL_TERMINAL_PROCESSING)` on startup |
| 5 | Scheduler daemon mode | `sm schedule daemon` for environments without external cron / Task Scheduler |

---

## Architecture

```
src/
├── main.rs              Entry point, tokio runtime
├── platform/            OS abstraction (StepKind executor, path resolution, shell detection)
├── registry/            Dual-store command registry (curated + staging), promotion policy, redb cache
├── embedder/            Flat binary embedding store; BoW default, fastembed (MiniLM) optional
├── session/             Ring buffer of resolved + native commands (last 200)
├── disambiguator/       Re-ranking and interactive disambiguation prompt
├── resolver/            4-stage resolution pipeline (redb fast path when warm)
├── model_client/        OpenAI-compatible inference client (curl subprocess)
├── editor/              $EDITOR / $VISUAL integration, reload + re-embed on save
├── scheduler/           Cron-driven invocation of registered verbs
└── cli/                 All user-facing command handlers
```

See [REFERENCE.md](REFERENCE.md) for the full public API, resolution pipeline trace, binary format spec, and build notes.

---

## Build notes

(Build modes and feature combinations live in the [Setup](#setup) section
above. This section covers what's still stubbed and the legacy version
pinning.)

**Pinned dependencies:** Most `Cargo.toml` entries still carry exact `=`
pins inherited from a Rust 1.75 sandbox. They build cleanly on modern
toolchains but the pins are noise — strip them at your leisure. `redb`,
`fastembed`, and `cron` are already unpinned.

**Crates still stubbed out** (upgrade when convenient):

- **`reqwest`** — would replace the `curl` subprocess in `model_client`
  and unblock real HTTP for `StepKind::HttpCall`. Currently HTTP steps
  log a warning and return failure.
- **`shellexpand`** — would replace the manual `expand_env()` in
  `platform/mod.rs`.
- **`rustyline`** — would replace `std::io::stdin` readline in
  `cli/mod.rs` for history + tab completion in interactive prompts
  (`sm add proc`, `sm wrap`, disambiguator).

---

## License

MIT
