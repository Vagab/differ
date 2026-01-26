//! differ - Syntactic diff viewer with persistent line-level annotations
//!
//! A CLI tool for viewing diffs with the ability to add persistent annotations
//! stored in SQLite. Primary use case: annotating code changes for future AI
//! coding sessions.

mod config;
mod diff;
mod export;
mod storage;
mod syntax;
mod tui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

const REATTACH_CONTEXT_LINES: usize = 2;

use crate::config::Config;
use crate::diff::{find_repo_root, DiffEngine, DiffMode};
use crate::export::{export, ExportFormat};
use crate::storage::{AnnotationType, Side, Storage};

/// Parsed git diff arguments
#[derive(Debug)]
struct DiffArgs {
    mode: DiffMode,
    paths: Vec<String>,
}

/// Check if args look like git external diff format:
/// path old-file old-hex old-mode new-file new-hex new-mode
fn is_git_external_diff_args(args: &[String]) -> bool {
    // Git external diff passes 7 arguments
    args.len() == 7 && args[2].len() == 40 && args[5].len() == 40
}

/// Parse git diff-style arguments
fn parse_diff_args(args: &[String], staged: bool) -> DiffArgs {
    // Check if this is git external diff format
    if is_git_external_diff_args(args) {
        return DiffArgs {
            mode: DiffMode::ExternalDiff {
                path: args[0].clone(),
                old_file: args[1].clone(),
                new_file: args[4].clone(),
            },
            paths: Vec::new(),
        };
    }

    // Find -- separator for path filtering
    let (rev_args, paths): (Vec<_>, Vec<_>) = if let Some(pos) = args.iter().position(|a| a == "--") {
        (args[..pos].to_vec(), args[pos + 1..].to_vec())
    } else {
        (args.to_vec(), Vec::new())
    };

    let mode = if staged {
        // --staged: index vs HEAD
        DiffMode::Staged
    } else if rev_args.is_empty() {
        // No args: working tree vs index (unstaged changes)
        DiffMode::Unstaged
    } else if rev_args.len() == 1 {
        let arg = &rev_args[0];
        // Check for .. or ... syntax
        if let Some(pos) = arg.find("...") {
            let (from, to) = arg.split_at(pos);
            let to = &to[3..]; // skip "..."
            DiffMode::MergeBase {
                from: from.to_string(),
                to: to.to_string(),
            }
        } else if let Some(pos) = arg.find("..") {
            let (from, to) = arg.split_at(pos);
            let to = &to[2..]; // skip ".."
            DiffMode::Commits {
                from: from.to_string(),
                to: to.to_string(),
            }
        } else {
            // Single revision: working tree vs that revision
            DiffMode::WorkingTree { base: arg.clone() }
        }
    } else {
        // Two revisions
        DiffMode::Commits {
            from: rev_args[0].clone(),
            to: rev_args[1].clone(),
        }
    };

    DiffArgs {
        mode,
        paths: paths.into_iter().map(String::from).collect(),
    }
}

#[derive(Parser)]
#[command(name = "differ")]
#[command(about = "TUI diff viewer with persistent annotations")]
#[command(long_about = "A drop-in replacement for git diff with an interactive TUI and \
    the ability to add persistent annotations to code changes.\n\n\
    Use 'git d' as an alias by adding to ~/.gitconfig:\n\
    [alias]\n    \
    d = ! /path/to/differ diff")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// View diff interactively with TUI (accepts git diff arguments)
    ///
    /// Examples:
    ///   differ diff                    # working tree vs index (unstaged changes)
    ///   differ diff --staged           # index vs HEAD (staged changes)
    ///   differ diff HEAD               # working tree vs HEAD
    ///   differ diff main..feature      # between two branches
    ///   differ diff abc123 def456      # between two commits
    ///   differ diff HEAD -- src/       # only files in src/
    Diff {
        /// Show staged changes (index vs HEAD)
        #[arg(long, visible_alias = "cached")]
        staged: bool,

        /// Enable side-by-side view
        #[arg(short = 's', long)]
        side_by_side: bool,

        /// Number of context lines around changes
        #[arg(short = 'c', long)]
        context_lines: Option<u32>,

        /// Git diff arguments: [<commit>] [<commit>] [-- <path>...]
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// List all annotations for the current repository
    List {
        /// Filter by file path
        #[arg(short, long)]
        file: Option<String>,
    },

    /// Add an annotation from the command line
    Add {
        /// File path (relative to repo root)
        #[arg(short, long)]
        file: String,

        /// Line number
        #[arg(short, long)]
        line: u32,

        /// End line (for multi-line annotations)
        #[arg(long)]
        end_line: Option<u32>,

        /// Annotation type: comment or todo
        #[arg(short = 't', long, default_value = "comment")]
        annotation_type: String,

        /// Annotation content
        content: String,
    },

    /// Export annotations to markdown or JSON (useful for AI context)
    Export {
        /// Export format: markdown (md) or json
        #[arg(short, long, default_value = "markdown")]
        format: String,

        /// Output file (default: stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Clear all annotations for the current repository
    Clear,

    /// Open config file in $EDITOR
    Config,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Find repo root and initialize storage
    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let repo_path = find_repo_root(&cwd)?;
    let storage = Storage::open_default()?;

    // Get display name from repo directory name
    let display_name = repo_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string());

    let repo_id = storage.get_or_create_repo(&repo_path, display_name.as_deref())?;

    // Load config and apply CLI overrides
    let config = Config::load().unwrap_or_default();

    match cli.command {
        Commands::Diff {
            staged,
            side_by_side,
            context_lines,
            args,
        } => {
            let config = config.with_overrides(
                if side_by_side { Some(true) } else { None },
                context_lines,
            );

            // Parse git diff-style arguments
            let diff_args = parse_diff_args(&args, staged);
            cmd_diff(&storage, &repo_path, repo_id, diff_args, config)?;
        }
        Commands::List { file } => {
            cmd_list(&storage, repo_id, file.as_deref())?;
        }
        Commands::Add {
            file,
            line,
            end_line,
            annotation_type,
            content,
        } => {
            cmd_add(
                &storage,
                &repo_path,
                repo_id,
                &file,
                line,
                end_line,
                &annotation_type,
                &content,
            )?;
        }
        Commands::Export { format, output } => {
            cmd_export(&storage, repo_id, &format, output)?;
        }
        Commands::Clear => {
            cmd_clear(&storage, repo_id)?;
        }
        Commands::Config => {
            cmd_config()?;
        }
    }

    Ok(())
}

fn cmd_diff(
    _storage: &Storage,
    repo_path: &PathBuf,
    repo_id: i64,
    args: DiffArgs,
    config: Config,
) -> Result<()> {
    let diff_engine = DiffEngine::new(repo_path.clone(), config.context_lines);

    let files = diff_engine.diff(&args.mode, &args.paths)?;

    if files.is_empty() {
        let msg = match &args.mode {
            DiffMode::Unstaged => "No unstaged changes",
            DiffMode::Staged => "No staged changes",
            DiffMode::WorkingTree { base } => &format!("No changes against {}", base),
            DiffMode::Commits { from, to } => &format!("No changes between {} and {}", from, to),
            DiffMode::MergeBase { from, to } => &format!("No changes between {} and {} (merge-base)", from, to),
            DiffMode::ExternalDiff { path, .. } => &format!("No changes in {}", path),
        };
        println!("{}", msg);
        return Ok(());
    }

    // Clone storage for TUI (it needs ownership)
    let tui_storage = Storage::open_default()?;

    tui::run(tui_storage, diff_engine, repo_path.clone(), repo_id, files, config)
}

fn cmd_list(
    storage: &Storage,
    repo_id: i64,
    file: Option<&str>,
) -> Result<()> {
    let annotations = storage.list_annotations(repo_id, file)?;

    if annotations.is_empty() {
        println!("No annotations found");
        return Ok(());
    }

    let mut current_file = String::new();

    for annotation in annotations {
        if annotation.file_path != current_file {
            if !current_file.is_empty() {
                println!();
            }
            println!("{}:", annotation.file_path);
            current_file = annotation.file_path.clone();
        }

        let type_marker = match annotation.annotation_type {
            AnnotationType::Comment => "[C]",
            AnnotationType::Todo => "[T]",
        };

        let line_info = if let Some(end) = annotation.end_line {
            format!("L{}-{}", annotation.start_line, end)
        } else {
            format!("L{}", annotation.start_line)
        };

        let side = match annotation.side {
            Side::Old => " (old)",
            Side::New => "",
        };

        println!(
            "  #{} {} {}{}: {}",
            annotation.id,
            type_marker,
            line_info,
            side,
            annotation.content
        );
    }

    Ok(())
}

fn cmd_add(
    storage: &Storage,
    repo_path: &PathBuf,
    repo_id: i64,
    file: &str,
    line: u32,
    end_line: Option<u32>,
    annotation_type: &str,
    content: &str,
) -> Result<()> {
    let atype = AnnotationType::from_str(annotation_type)
        .context("Invalid annotation type. Use: comment or todo")?;

    let (anchor_line, anchor_text, context_before, context_after) =
        build_anchor_from_file(repo_path, file, line);

    let id = storage.add_annotation(
        repo_id,
        file,
        None, // commit_sha
        Side::New,
        line,
        end_line,
        atype,
        content,
        anchor_line,
        &anchor_text,
        &context_before,
        &context_after,
    )?;

    println!("Added annotation #{}", id);
    Ok(())
}

fn build_anchor_from_file(
    repo_path: &PathBuf,
    file: &str,
    line: u32,
) -> (u32, String, String, String) {
    let full_path = repo_path.join(file);
    let content = match std::fs::read_to_string(&full_path) {
        Ok(content) => content,
        Err(_) => return (line, String::new(), String::new(), String::new()),
    };

    let lines: Vec<&str> = content.lines().collect();
    if line == 0 || lines.is_empty() || (line as usize) > lines.len() {
        return (line, String::new(), String::new(), String::new());
    }

    let idx = line.saturating_sub(1) as usize;
    let anchor_text = lines.get(idx).copied().unwrap_or("").to_string();
    let start = idx.saturating_sub(REATTACH_CONTEXT_LINES);
    let before = lines[start..idx].join("\n");
    let after_end = (idx + 1 + REATTACH_CONTEXT_LINES).min(lines.len());
    let after = if idx + 1 < after_end {
        lines[idx + 1..after_end].join("\n")
    } else {
        String::new()
    };

    (line, anchor_text, before, after)
}

fn cmd_export(
    storage: &Storage,
    repo_id: i64,
    format: &str,
    output: Option<PathBuf>,
) -> Result<()> {
    let export_format = ExportFormat::from_str(format)
        .context("Invalid format. Use: markdown (md) or json")?;

    let content = export(storage, repo_id, export_format)?;

    if let Some(path) = output {
        std::fs::write(&path, &content)
            .with_context(|| format!("Failed to write to {}", path.display()))?;
        println!("Exported to {}", path.display());
    } else {
        print!("{}", content);
    }

    Ok(())
}

fn cmd_clear(storage: &Storage, repo_id: i64) -> Result<()> {
    let count = storage.clear_all(repo_id)?;
    println!("Cleared {} annotations", count);
    Ok(())
}

fn cmd_config() -> Result<()> {
    let config_path = Config::default_path();

    // Create default config if it doesn't exist
    if !config_path.exists() {
        Config::create_default()?;
    }

    // Get editor from $EDITOR, fall back to vi
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());

    // Open in editor
    std::process::Command::new(&editor)
        .arg(&config_path)
        .status()
        .with_context(|| format!("Failed to open editor: {}", editor))?;

    Ok(())
}
