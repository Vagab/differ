# differ

A TUI diff viewer with persistent annotations. Drop-in replacement for `git diff` with the ability to annotate code changes.

## Features

- **Git-compatible CLI** - accepts same arguments as `git diff`
- **Interactive TUI** with vim-style navigation
- **Persistent annotations** stored in SQLite
- **Full file expansion** - view entire file with changes highlighted
- **Side-by-side view** (optional)
- **Export annotations** to Markdown/JSON for AI context

## Installation

```bash
./install.sh
```

This builds the binary, installs it to `~/.local/bin`, and adds it to your PATH.

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

### Navigation
| Key | Action |
|-----|--------|
| `j/k` | Move up/down |
| `n/N` | Next/prev file (or chunk in expanded mode) |
| `]/[` | Next/prev change |
| `Ctrl+d/u` | Half page down/up |
| `g/G` | Go to top/bottom |

### View
| Key | Action |
|-----|--------|
| `x` | Expand/collapse file (full file view) |
| `s` | Toggle side-by-side view |
| `c` | Toggle annotation visibility |

### Annotations
| Key | Action |
|-----|--------|
| `a` | Add annotation |
| `e` | Edit annotation |
| `d` | Delete annotation |
| `t` | Toggle type (comment/todo) |

### Other
| Key | Action |
|-----|--------|
| `?` | Help |
| `q` | Quit |

## Config

`~/.config/differ/config.toml`:

```toml
side_by_side = false
context_lines = 3
show_annotations = true
```

## License

MIT
