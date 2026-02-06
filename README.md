# differ

A TUI diff viewer with persistent annotations. Drop-in replacement for `git diff` with the ability to annotate code changes.

```
    .___.__  _____  _____             
  __| _/|__|/ ____\/ ____\___________ 
 / __ | |  \   __\\   __\/ __ \_  __ \
/ /_/ | |  ||  |   |  | \  ___/|  | \/
\____ | |__||__|   |__|  \___  >__|   
     \/                      \/       
```

## Features

- **Git-compatible CLI** (works like `git diff`)
- **Interactive TUI** with vim-style navigation
- **Persistent annotations** stored in SQLite (comment/todo + resolve)
- **Expanded file view** (full file, with changes highlighted)
- **Side-by-side view** and syntax highlighting
- **Staging/unstaging** hunks, **discard** hunk
- **Sidebar** with modified/added/deleted files
- **Auto-reload** via filesystem watcher
- **Export annotations** to Markdown/JSON for AI context

## Installation

Requires [Rust](https://rustup.rs/). Then:

```bash
cargo install differ_cli --locked
```

Binary is installed to `~/.cargo/bin` as `differ` (ensure it’s in your PATH).

Or build locally:

```bash
./install.sh
```

## Git Setup

Add to your `~/.gitconfig`:

```gitconfig
[alias]
    d = ! /path/to/differ diff
    ds = ! /path/to/differ diff --staged
```

Then use `git d` instead of `git diff`.

## Usage

```bash
differ diff                    # unstaged changes (like git diff)
differ diff --staged           # staged changes (like git diff --staged)
differ diff HEAD               # working tree vs HEAD
differ diff main..feature      # between branches
differ diff HEAD~3..HEAD       # last 3 commits
differ diff -- src/            # filter by path
```

### Annotations

```bash
# In TUI: press 'a' to add annotation at current line

# CLI commands
differ add -f src/main.rs -l 42 "needs refactoring"
differ list
differ export                  # markdown to stdout
differ export -f json          # JSON format
differ clear                   # clear all annotations
```

## Keybindings

Press `?` in the TUI for the full list. Highlights:

- `j/k`, `n/N`, `Tab`/`Shift+Tab`, `g/G` for navigation
- `x` expand, `c` collapse, `v` side-by-side, `B` sidebar, `b` focus sidebar
- `s` stage/unstage hunk, `D` discard hunk (unstaged)
- `a/e/d/r/t` annotations (add/edit/delete/resolve/type)
- `R` reload, `@` send annotation to AI

## Config

`~/.config/differ/config.toml`:

```toml
side_by_side = false
context_lines = 3
show_annotations = true
syntax_highlighting = true
ai_target = "claude" # or "codex"
watch_ignore_paths = [".git", "target", "_build", "deps"]
```

## License

MIT
