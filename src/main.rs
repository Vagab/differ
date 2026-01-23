//! differ - Syntactic diff viewer with persistent line-level annotations
//!
//! A CLI tool for viewing diffs with the ability to add persistent annotations
//! stored in SQLite. Primary use case: annotating code changes for future AI
//! coding sessions.

mod diff;
mod export;
mod storage;
mod tui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use crate::diff::{find_repo_root, DiffEngine};
use crate::export::{export, ExportFormat};
use crate::storage::{AnnotationType, Side, Storage};

#[derive(Parser)]
#[command(name = "differ")]
#[command(about = "Syntactic diff viewer with persistent annotations")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// View diff interactively with TUI
    Diff {
        /// Base revision (default: HEAD)
        #[arg(default_value = "HEAD~1")]
        from: String,

        /// Target revision (default: HEAD)
        #[arg(default_value = "HEAD")]
        to: String,
    },

    /// List annotations for the repository
    List {
        /// Filter by file path
        #[arg(short, long)]
        file: Option<String>,

        /// Include resolved annotations
        #[arg(long)]
        resolved: bool,
    },

    /// Add an annotation (non-interactive)
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

        /// Annotation type: comment, todo, ai_prompt
        #[arg(short = 't', long, default_value = "comment")]
        annotation_type: String,

        /// Annotation content
        content: String,
    },

    /// Export annotations for AI context
    Export {
        /// Export format: markdown (md) or json
        #[arg(short, long, default_value = "markdown")]
        format: String,

        /// Include resolved annotations
        #[arg(long)]
        resolved: bool,

        /// Output file (default: stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Clear resolved annotations
    Clear {
        /// Clear all annotations (not just resolved)
        #[arg(long)]
        all: bool,
    },
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

    match cli.command {
        Commands::Diff { from, to } => {
            cmd_diff(&storage, &repo_path, repo_id, &from, &to)?;
        }
        Commands::List { file, resolved } => {
            cmd_list(&storage, repo_id, file.as_deref(), resolved)?;
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
                repo_id,
                &file,
                line,
                end_line,
                &annotation_type,
                &content,
            )?;
        }
        Commands::Export {
            format,
            resolved,
            output,
        } => {
            cmd_export(&storage, repo_id, &format, resolved, output)?;
        }
        Commands::Clear { all } => {
            cmd_clear(&storage, repo_id, all)?;
        }
    }

    Ok(())
}

fn cmd_diff(
    storage: &Storage,
    repo_path: &PathBuf,
    repo_id: i64,
    from: &str,
    to: &str,
) -> Result<()> {
    let diff_engine = DiffEngine::new(repo_path.clone());
    let files = diff_engine.get_changed_files(from, to)?;

    if files.is_empty() {
        println!("No changes between {} and {}", from, to);
        return Ok(());
    }

    // Clone storage for TUI (it needs ownership)
    let tui_storage = Storage::open_default()?;

    tui::run(tui_storage, diff_engine, repo_path.clone(), repo_id, files)
}

fn cmd_list(
    storage: &Storage,
    repo_id: i64,
    file: Option<&str>,
    include_resolved: bool,
) -> Result<()> {
    let annotations = storage.list_annotations(repo_id, file, include_resolved)?;

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
            AnnotationType::AiPrompt => "[A]",
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

        let resolved = if annotation.resolved_at.is_some() {
            " [resolved]"
        } else {
            ""
        };

        println!(
            "  #{} {} {}{}{}: {}",
            annotation.id,
            type_marker,
            line_info,
            side,
            resolved,
            annotation.content
        );
    }

    Ok(())
}

fn cmd_add(
    storage: &Storage,
    repo_id: i64,
    file: &str,
    line: u32,
    end_line: Option<u32>,
    annotation_type: &str,
    content: &str,
) -> Result<()> {
    let atype = AnnotationType::from_str(annotation_type)
        .context("Invalid annotation type. Use: comment, todo, or ai_prompt")?;

    let id = storage.add_annotation(
        repo_id,
        file,
        None, // commit_sha
        Side::New,
        line,
        end_line,
        atype,
        content,
    )?;

    println!("Added annotation #{}", id);
    Ok(())
}

fn cmd_export(
    storage: &Storage,
    repo_id: i64,
    format: &str,
    include_resolved: bool,
    output: Option<PathBuf>,
) -> Result<()> {
    let export_format = ExportFormat::from_str(format)
        .context("Invalid format. Use: markdown (md) or json")?;

    let content = export(storage, repo_id, export_format, include_resolved)?;

    if let Some(path) = output {
        std::fs::write(&path, &content)
            .with_context(|| format!("Failed to write to {}", path.display()))?;
        println!("Exported to {}", path.display());
    } else {
        print!("{}", content);
    }

    Ok(())
}

fn cmd_clear(storage: &Storage, repo_id: i64, all: bool) -> Result<()> {
    if all {
        // Clear all annotations - need to implement this
        let annotations = storage.list_annotations(repo_id, None, true)?;
        for annotation in annotations {
            storage.delete_annotation(annotation.id)?;
        }
        println!("Cleared all annotations");
    } else {
        let count = storage.clear_resolved(repo_id)?;
        println!("Cleared {} resolved annotations", count);
    }

    Ok(())
}
// Testing diff
