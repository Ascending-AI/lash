# lash

AI coding agent with a persistent Python REPL, hashline-safe file editing, and a host-friendly runtime API for TUI and headless frontends.

## Setup

```bash
cat > ~/.local/bin/lash << 'SCRIPT'
#!/bin/bash
cargo build --release --manifest-path ~/code/lash/Cargo.toml 2>/dev/null
exec ~/code/lash/target/release/lash "$@"
SCRIPT
chmod +x ~/.local/bin/lash
```

## Python Build Modes

`lash`/`lash-core` support two PyO3 build modes:

- `python-system` (default): link against host Python discovered by PyO3 (and provide `dill` in that Python env).
- `python-bundled`: link against `python-build-standalone` and bundle stdlib+dill.

### Local dev

```bash
./dev.sh                           # default: system Python
LASH_PYTHON_MODE=bundled ./dev.sh  # bundled python-build-standalone
```

### Build commands

```bash
# System Python
cargo xtask build --python system

# Bundled Python (auto-bootstrap + auto-configure)
cargo xtask build --python bundled
```

`cargo xtask build --python bundled` bootstraps standalone Python as needed and sets
`PYO3_CONFIG_FILE` automatically for the build invocation.

Manual bundled build (advanced/CI override):

```bash
./scripts/fetch-python.sh
PYO3_CONFIG_FILE=$PWD/target/python-standalone/pyo3-config.txt cargo build -p lash-cli --features python-bundled
```

`PYO3_CONFIG_FILE` only affects build/link time; it is not a runtime requirement.
`python-system` mode expects `dill` to be importable at runtime (`python3 -m pip install dill`).

### Downstream (`lash-core`) integration

```toml
[dependencies]
lash-core = { path = "../lash", default-features = false, features = ["full", "python-system"] }
```

For bundled mode in downstream projects:

```toml
[dependencies]
lash-core = { path = "../lash", default-features = false, features = ["full", "python-bundled"] }
```

Then bootstrap bundled Python and set `PYO3_CONFIG_FILE` before running Cargo (same as above).

## CLI Usage

```bash
lash                               # start interactive TUI session
lash -p "summarize this repo"      # headless mode: run one prompt and print result
lash --print "summarize this repo"  # same, long form
lash --model gpt-5.3-codex         # override model
lash --no-mouse                    # disable mouse (re-enables terminal text selection)
lash --provider                    # force provider setup flow
lash --reset                       # delete ~/.lash and ~/.cache/lash, then exit
```

## Prompt Customization

Override system prompt sections at launch:

```bash
lash --prompt-replace "section=replacement text"
lash --prompt-replace-file "section=path/to/file.md"
lash --prompt-prepend "section=prepended text"
lash --prompt-prepend-file "section=path/to/file.md"
lash --prompt-append "section=appended text"
lash --prompt-append-file "section=path/to/file.md"
lash --prompt-disable "section"
```

## Skills

Skills are modular directories that extend lash with specialized knowledge and workflows.

Skill directories: `~/.lash/skills/` (global) and `.lash/skills/` (project-local, overrides global).

Each skill is a directory containing a `SKILL.md` with YAML frontmatter (`name`, `description`) and a markdown body. Supporting files (scripts, references, templates) are included alongside.

```
~/.lash/skills/
  my-skill/
    SKILL.md          # required: frontmatter + instructions
    scripts/foo.py    # optional supporting files
    references/bar.md
```

Use `/skills` to browse, `/<skill-name>` to invoke, or `load_skill("name")` from the REPL.

## Terminal Bench

Use `scripts/run-terminalbench.sh` to run Harbor + Terminal Bench 2 with the in-repo lash adapter.

By default it runs `terminal-bench-sample@2.0`, builds a glibc-compatible binary in `target-bookworm/release/lash`, and uses your local `~/.lash/config.json` inside benchmark containers (so OAuth/OpenRouter config is reused).

```bash
scripts/run-terminalbench.sh --sample
scripts/run-terminalbench.sh --full --task "git-*"
scripts/run-terminalbench.sh --sample --task chess-best-move --model gpt-5.3-codex
scripts/run-terminalbench.sh --sample --build-mode host   # use host binary instead
```

## Slash Commands

| Command | Description |
|---|---|
| `/clear`, `/new` | Reset conversation |
| `/controls` | Show keyboard shortcuts |
| `/model <name>` | Switch LLM model |
| `/provider` | Open provider setup in-app |
| `/login` | Alias for `/provider` |
| `/logout` | Remove stored credentials from disk |
| `/retry` | Replay the previous turn payload exactly |
| `/resume [name]` | Browse/load previous sessions |
| `/skills` | Browse loaded skills |
| `/tools` | Inspect or edit dynamic tools |
| `/caps` | Inspect or edit dynamic capabilities |
| `/reconfigure` | Apply or inspect pending runtime reconfigure |
| `/help`, `/?` | Show help |
| `/exit`, `/quit` | Quit |

`/logout` only clears persisted config. The active session may continue with in-memory credentials until you switch provider or restart.

## Plan Mode

Plan mode is a first-class runtime mode used for plan-then-execute workflows.

- The agent enters plan mode by calling `enter_plan_mode`.
- The TUI switches to plan mode and allocates a plan file in `.lash/plans/`.
- Runtime injects plan-mode guardrails (read/explore/design, no project file edits) and points the model at that plan file.
- `exit_plan_mode` returns plan content for user approval.
- After approval, lash clears message history for a fresh execution phase while preserving REPL state.

## Keyboard Shortcuts

| Key | Action |
|---|---|
| `Esc` | Cancel running agent / dismiss prompt |
| `Shift+Enter` | Insert newline |
| `Ctrl+U` / `Ctrl+D` | Scroll half-page up / down |
| `PgUp` / `PgDn` | Scroll full page |
| `Ctrl+V` | Paste image as inline `[Image #n]` |
| `Ctrl+Shift+V` | Paste text only |
| `Ctrl+Y` | Copy last response |
| `Ctrl+O` | Cycle tool expansion level |
| `Alt+O` | Full expansion (code + stdout) |

## Provider Defaults

Default root model by provider:

| Provider | Default model |
|---|---|
| `OpenRouter` | `anthropic/claude-sonnet-4.6` |
| `Claude` | `claude-opus-4-6` |
| `Codex` | `gpt-5.3-codex` |
| `Google OAuth` | `gemini-3.1-pro-preview` |

Default sub-agent tier mapping (`low` / `medium` / `high`):

| Provider | low | medium | high |
|---|---|---|---|
| `OpenRouter` | `minimax/minimax-m2.5` | `z-ai/glm-5` | `anthropic/claude-sonnet-4.6` |
| `Claude` | `claude-haiku-4-5` | `claude-sonnet-4-6` | `claude-sonnet-4-6` |
| `Codex` | `gpt-5.3-codex-spark` | `gpt-5.3-codex` (`reasoning=medium`) | `gpt-5.3-codex` (`reasoning=high`) |
| `Google OAuth` | `gemini-3-flash-preview` | `gemini-3.1-pro-preview` | `gemini-3.1-pro-preview` |

## Frontend / Runtime Integration

`lash-core` exposes a unified turn input surface for third-party frontends (`lash/src/runtime.rs`).

- `TurnInput.items` is an ordered list of typed items.
- Supported item kinds:
  - `text`
  - `file_ref`
  - `dir_ref`
  - `image_ref`
  - `skill_ref`
- `TurnInput.image_blobs` carries raw image bytes keyed by `image_ref.id`.

This preserves interleaving and intent, e.g. text -> image -> text -> file reference in a single turn.

### Example payload

```json
{
  "items": [
    {"type": "text", "text": "Please update this"},
    {"type": "image_ref", "id": "img-1"},
    {"type": "text", "text": "and check"},
    {"type": "file_ref", "path": "/repo/src/main.rs"}
  ],
  "image_blobs": {
    "img-1": "<binary png bytes>"
  }
}
```

## Tool Exposure Model

Tools define `inject_into_prompt: bool` (`lash/src/lib.rs`).

- `inject_into_prompt=true`: tool appears directly in the LLM system prompt.
- `inject_into_prompt=false`: tool is omitted from prompt for brevity, but still callable at runtime.

Runtime discovery is available via:

- `list_tools(...)`
- `search_tools(query, mode="hybrid", ...)`

The REPL exposes the full tool namespace under `tools` (for example `tools.read_file(...)`, `tools.search_tools(...)`).
It also implicitly runs `from tools import *`, so visible tools can be called directly.

## Filesystem Listing Output

`glob(...)` and `ls(...)` return typed filesystem entries (`PathEntry`) rather than plain path/tree strings.

Each item includes:

- `path`
- `kind` (`file` / `dir` / `symlink` / `other`)
- `size_bytes`
- `lines` (`null` unless `with_lines=true`)
- `modified_at` (RFC3339 UTC)

Both tools support `limit` (`null` for uncapped) and expose truncation metadata on the returned object in REPL wrappers.

All rights reserved.
