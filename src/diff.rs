//! Diff engine with difftastic integration and git fallback
//!
//! Primary: difftastic subprocess with JSON output
//! Fallback: git diff if difftastic is not available

use anyhow::{Context, Result};
use git2::{DiffOptions, Repository};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Represents a changed file in a diff
#[derive(Debug, Clone)]
pub struct DiffFile {
    pub old_path: Option<PathBuf>,
    pub new_path: Option<PathBuf>,
    pub status: FileStatus,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

/// A hunk of changes within a file
#[derive(Debug, Clone)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<DiffLine>,
}

/// A single line in a diff
#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_line_no: Option<u32>,
    pub new_line_no: Option<u32>,
    pub content: String,
    /// For syntactic diffs, highlighted ranges within the line
    pub highlights: Vec<HighlightRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Addition,
    Deletion,
}

#[derive(Debug, Clone)]
pub struct HighlightRange {
    pub start: usize,
    pub end: usize,
}

/// Check if difftastic is available
pub fn difftastic_available() -> bool {
    Command::new("difft")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Diff engine that can use difftastic or fall back to git
pub struct DiffEngine {
    use_difftastic: bool,
    repo_path: PathBuf,
}

impl DiffEngine {
    pub fn new(repo_path: PathBuf) -> Self {
        let use_difftastic = difftastic_available();
        if !use_difftastic {
            eprintln!(
                "Warning: difftastic not found. Using basic git diff. \
                 Install with: cargo install difftastic"
            );
        }
        Self {
            use_difftastic,
            repo_path,
        }
    }

    /// Gets the list of changed files between two revisions
    pub fn get_changed_files(&self, from: &str, to: &str) -> Result<Vec<DiffFile>> {
        let repo = Repository::open(&self.repo_path)
            .context("Failed to open git repository")?;

        let from_tree = self.resolve_tree(&repo, from)?;
        let to_tree = self.resolve_tree(&repo, to)?;

        let mut opts = DiffOptions::new();

        let mut diff = repo
            .diff_tree_to_tree(Some(&from_tree), Some(&to_tree), Some(&mut opts))
            .context("Failed to create diff")?;

        // Enable rename detection
        let mut find_opts = git2::DiffFindOptions::new();
        find_opts.renames(true);
        find_opts.copies(true);
        diff.find_similar(Some(&mut find_opts))?;

        let mut files = Vec::new();

        diff.foreach(
            &mut |delta, _| {
                let status = match delta.status() {
                    git2::Delta::Added => FileStatus::Added,
                    git2::Delta::Deleted => FileStatus::Deleted,
                    git2::Delta::Modified => FileStatus::Modified,
                    git2::Delta::Renamed => FileStatus::Renamed,
                    _ => return true, // Skip other statuses
                };

                let old_path = delta.old_file().path().map(PathBuf::from);
                let new_path = delta.new_file().path().map(PathBuf::from);

                files.push(DiffFile {
                    old_path,
                    new_path,
                    status,
                    hunks: Vec::new(), // Will be populated later
                });

                true
            },
            None,
            None,
            None,
        )?;

        // Populate hunks for each file
        for file in &mut files {
            if let Some(ref new_path) = file.new_path {
                if self.use_difftastic {
                    if let Ok(hunks) = self.get_difftastic_hunks(&repo, from, to, new_path) {
                        file.hunks = hunks;
                        continue;
                    }
                }
                // Fallback to git diff
                file.hunks = self.get_git_hunks(&repo, from, to, file)?;
            }
        }

        Ok(files)
    }

    /// Diff working tree against a revision
    pub fn diff_working_tree(&self, base: &str) -> Result<Vec<DiffFile>> {
        let repo = Repository::open(&self.repo_path)
            .context("Failed to open git repository")?;

        let base_tree = self.resolve_tree(&repo, base)?;

        let mut opts = DiffOptions::new();

        let mut diff = repo
            .diff_tree_to_workdir_with_index(Some(&base_tree), Some(&mut opts))
            .context("Failed to create diff")?;

        // Enable rename detection
        let mut find_opts = git2::DiffFindOptions::new();
        find_opts.renames(true);
        diff.find_similar(Some(&mut find_opts))?;

        let mut files = Vec::new();

        diff.foreach(
            &mut |delta, _| {
                let status = match delta.status() {
                    git2::Delta::Added => FileStatus::Added,
                    git2::Delta::Deleted => FileStatus::Deleted,
                    git2::Delta::Modified => FileStatus::Modified,
                    git2::Delta::Renamed => FileStatus::Renamed,
                    _ => return true,
                };

                let old_path = delta.old_file().path().map(PathBuf::from);
                let new_path = delta.new_file().path().map(PathBuf::from);

                files.push(DiffFile {
                    old_path,
                    new_path,
                    status,
                    hunks: Vec::new(),
                });

                true
            },
            None,
            None,
            None,
        )?;

        // Populate hunks
        for file in &mut files {
            file.hunks = self.get_git_hunks(&repo, base, "HEAD", file)?;
        }

        Ok(files)
    }

    fn resolve_tree<'a>(
        &self,
        repo: &'a Repository,
        rev: &str,
    ) -> Result<git2::Tree<'a>> {
        let obj = repo
            .revparse_single(rev)
            .with_context(|| format!("Failed to resolve revision: {}", rev))?;

        let commit = obj
            .peel_to_commit()
            .with_context(|| format!("Failed to get commit for: {}", rev))?;

        commit
            .tree()
            .context("Failed to get tree from commit")
    }

    fn get_difftastic_hunks(
        &self,
        repo: &Repository,
        from: &str,
        to: &str,
        file_path: &Path,
    ) -> Result<Vec<DiffHunk>> {
        // Get the file contents at both revisions
        let old_content = self.get_file_at_rev(repo, from, file_path)?;
        let new_content = self.get_file_at_rev(repo, to, file_path)?;

        // Write to temp files for difftastic
        let temp_dir = std::env::temp_dir();
        let old_file = temp_dir.join("differ_old");
        let new_file = temp_dir.join("differ_new");

        std::fs::write(&old_file, &old_content)?;
        std::fs::write(&new_file, &new_content)?;

        // Run difftastic with JSON output
        // SECURITY: Using arg() for each argument to avoid shell injection
        let output = Command::new("difft")
            .arg("--display")
            .arg("json")
            .arg(&old_file)
            .arg(&new_file)
            .output()
            .context("Failed to execute difftastic")?;

        // Clean up temp files
        let _ = std::fs::remove_file(&old_file);
        let _ = std::fs::remove_file(&new_file);

        if !output.status.success() {
            anyhow::bail!("difftastic failed");
        }

        // Parse difftastic JSON output
        self.parse_difftastic_json(&output.stdout, &old_content, &new_content)
    }

    fn get_file_at_rev(&self, repo: &Repository, rev: &str, path: &Path) -> Result<String> {
        let obj = repo.revparse_single(rev)?;
        let commit = obj.peel_to_commit()?;
        let tree = commit.tree()?;

        let entry = tree.get_path(path)?;
        let blob = repo.find_blob(entry.id())?;

        String::from_utf8(blob.content().to_vec())
            .context("File content is not valid UTF-8")
    }

    fn parse_difftastic_json(
        &self,
        json_bytes: &[u8],
        old_content: &str,
        new_content: &str,
    ) -> Result<Vec<DiffHunk>> {
        // Difftastic JSON output structure (simplified)
        #[derive(Deserialize)]
        struct DifftOutput {
            #[serde(default)]
            hunks: Vec<DifftHunk>,
        }

        #[derive(Deserialize)]
        struct DifftHunk {
            #[serde(default)]
            old_start: u32,
            #[serde(default)]
            new_start: u32,
            #[serde(default)]
            changes: Vec<DifftChange>,
        }

        #[derive(Deserialize)]
        struct DifftChange {
            #[serde(default)]
            kind: String,
            #[serde(default)]
            old_line: Option<u32>,
            #[serde(default)]
            new_line: Option<u32>,
            #[serde(default)]
            content: String,
        }

        // Try to parse as difftastic JSON
        if let Ok(output) = serde_json::from_slice::<DifftOutput>(json_bytes) {
            return Ok(output
                .hunks
                .into_iter()
                .map(|h| DiffHunk {
                    old_start: h.old_start,
                    old_lines: 0, // Will calculate
                    new_start: h.new_start,
                    new_lines: 0,
                    lines: h
                        .changes
                        .into_iter()
                        .map(|c| DiffLine {
                            kind: match c.kind.as_str() {
                                "add" | "addition" => LineKind::Addition,
                                "del" | "deletion" => LineKind::Deletion,
                                _ => LineKind::Context,
                            },
                            old_line_no: c.old_line,
                            new_line_no: c.new_line,
                            content: c.content,
                            highlights: Vec::new(),
                        })
                        .collect(),
                })
                .collect());
        }

        // If JSON parsing fails, fall back to creating hunks from raw content
        self.create_unified_hunks(old_content, new_content)
    }

    fn create_unified_hunks(&self, old: &str, new: &str) -> Result<Vec<DiffHunk>> {
        // Simple line-by-line diff as fallback
        let old_lines: Vec<&str> = old.lines().collect();
        let new_lines: Vec<&str> = new.lines().collect();

        let mut lines = Vec::new();

        // Very basic diff - just show all lines
        // In practice, we'd use a proper diff algorithm here
        for (i, line) in new_lines.iter().enumerate() {
            lines.push(DiffLine {
                kind: if old_lines.get(i) == Some(line) {
                    LineKind::Context
                } else if i >= old_lines.len() {
                    LineKind::Addition
                } else {
                    LineKind::Context
                },
                old_line_no: if i < old_lines.len() {
                    Some(i as u32 + 1)
                } else {
                    None
                },
                new_line_no: Some(i as u32 + 1),
                content: line.to_string(),
                highlights: Vec::new(),
            });
        }

        if lines.is_empty() {
            return Ok(Vec::new());
        }

        Ok(vec![DiffHunk {
            old_start: 1,
            old_lines: old_lines.len() as u32,
            new_start: 1,
            new_lines: new_lines.len() as u32,
            lines,
        }])
    }

    fn get_git_hunks(
        &self,
        repo: &Repository,
        from: &str,
        to: &str,
        file: &DiffFile,
    ) -> Result<Vec<DiffHunk>> {
        let from_tree = self.resolve_tree(repo, from)?;
        let to_tree = self.resolve_tree(repo, to)?;

        let mut opts = DiffOptions::new();
        if let Some(ref path) = file.new_path {
            opts.pathspec(path);
        } else if let Some(ref path) = file.old_path {
            opts.pathspec(path);
        }

        let diff = repo.diff_tree_to_tree(Some(&from_tree), Some(&to_tree), Some(&mut opts))?;

        // Use RefCell to allow interior mutability in closures
        use std::cell::RefCell;
        let hunks = RefCell::new(Vec::new());
        let current_hunk: RefCell<Option<DiffHunk>> = RefCell::new(None);

        diff.foreach(
            &mut |_, _| true,
            None,
            Some(&mut |_delta, hunk| {
                if let Some(h) = current_hunk.borrow_mut().take() {
                    hunks.borrow_mut().push(h);
                }
                *current_hunk.borrow_mut() = Some(DiffHunk {
                    old_start: hunk.old_start(),
                    old_lines: hunk.old_lines(),
                    new_start: hunk.new_start(),
                    new_lines: hunk.new_lines(),
                    lines: Vec::new(),
                });
                true
            }),
            Some(&mut |_delta, _hunk, line| {
                if let Some(ref mut h) = *current_hunk.borrow_mut() {
                    let kind = match line.origin() {
                        '+' => LineKind::Addition,
                        '-' => LineKind::Deletion,
                        _ => LineKind::Context,
                    };

                    let content = String::from_utf8_lossy(line.content()).to_string();

                    h.lines.push(DiffLine {
                        kind,
                        old_line_no: line.old_lineno(),
                        new_line_no: line.new_lineno(),
                        content,
                        highlights: Vec::new(),
                    });
                }
                true
            }),
        )?;

        if let Some(h) = current_hunk.into_inner() {
            hunks.borrow_mut().push(h);
        }

        Ok(hunks.into_inner())
    }
}

/// Find the git repository root from a path
pub fn find_repo_root(start: &Path) -> Result<PathBuf> {
    let repo = Repository::discover(start)
        .context("Not in a git repository")?;

    repo.workdir()
        .map(PathBuf::from)
        .context("Repository has no working directory")
}
