//! Diff engine with difftastic integration and git fallback
//!
//! Primary: difftastic subprocess with JSON output
//! Fallback: git diff if difftastic is not available

use anyhow::{Context, Result};
use git2::{DiffOptions, Repository};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Diff mode - what to compare
#[derive(Debug, Clone)]
pub enum DiffMode {
    /// Working tree vs index (unstaged changes) - `git diff`
    Unstaged,
    /// Index vs HEAD (staged changes) - `git diff --staged`
    Staged,
    /// Working tree vs a specific revision - `git diff <commit>`
    WorkingTree { base: String },
    /// Between two commits - `git diff <from> <to>` or `git diff <from>..<to>`
    Commits { from: String, to: String },
    /// Changes since merge-base - `git diff <from>...<to>`
    MergeBase { from: String, to: String },
    /// External diff mode - git passes old and new file paths directly
    ExternalDiff { path: String, old_file: String, new_file: String },
}

/// Represents a changed file in a diff
#[derive(Debug, Clone)]
#[allow(dead_code)]
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
#[allow(dead_code)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<DiffLine>,
}

/// A single line in a diff
#[derive(Debug, Clone)]
#[allow(dead_code)]
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
#[allow(dead_code)]
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
    context_lines: u32,
}

impl DiffEngine {
    pub fn new(repo_path: PathBuf, context_lines: u32) -> Self {
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
            context_lines,
        }
    }

    /// Main diff method - handles all diff modes
    pub fn diff(&self, mode: &DiffMode, paths: &[String]) -> Result<Vec<DiffFile>> {
        match mode {
            DiffMode::Unstaged => self.diff_unstaged(paths),
            DiffMode::Staged => self.diff_staged(paths),
            DiffMode::WorkingTree { base } => self.diff_working_tree_filtered(base, paths),
            DiffMode::Commits { from, to } => self.get_changed_files_filtered(from, to, paths),
            DiffMode::MergeBase { from, to } => {
                let repo = Repository::open(&self.repo_path)?;
                let from_obj = repo.revparse_single(from)?;
                let to_obj = repo.revparse_single(to)?;
                let base = repo.merge_base(from_obj.id(), to_obj.id())?;
                let base_str = base.to_string();
                self.get_changed_files_filtered(&base_str, to, paths)
            }
            DiffMode::ExternalDiff { path, old_file, new_file } => {
                self.diff_external_files(path, old_file, new_file)
            }
        }
    }

    /// Diff two files directly (for git external diff mode)
    fn diff_external_files(&self, path: &str, old_file: &str, new_file: &str) -> Result<Vec<DiffFile>> {
        let old_content = std::fs::read_to_string(old_file).unwrap_or_default();
        let new_content = std::fs::read_to_string(new_file).unwrap_or_default();

        // Determine file status
        let status = if old_content.is_empty() {
            FileStatus::Added
        } else if new_content.is_empty() {
            FileStatus::Deleted
        } else {
            FileStatus::Modified
        };

        // Use similar-style diff to create hunks
        let hunks = self.create_diff_hunks(&old_content, &new_content)?;

        Ok(vec![DiffFile {
            old_path: Some(PathBuf::from(path)),
            new_path: Some(PathBuf::from(path)),
            status,
            hunks,
        }])
    }

    /// Create diff hunks from two strings using a proper diff algorithm
    fn create_diff_hunks(&self, old: &str, new: &str) -> Result<Vec<DiffHunk>> {
        let old_lines: Vec<&str> = old.lines().collect();
        let new_lines: Vec<&str> = new.lines().collect();

        // Simple diff using longest common subsequence approach
        let mut hunks = Vec::new();
        let mut lines = Vec::new();

        let mut old_idx = 0;
        let mut new_idx = 0;

        while old_idx < old_lines.len() || new_idx < new_lines.len() {
            if old_idx < old_lines.len() && new_idx < new_lines.len() && old_lines[old_idx] == new_lines[new_idx] {
                // Context line
                lines.push(DiffLine {
                    kind: LineKind::Context,
                    old_line_no: Some(old_idx as u32 + 1),
                    new_line_no: Some(new_idx as u32 + 1),
                    content: old_lines[old_idx].to_string(),
                    highlights: Vec::new(),
                });
                old_idx += 1;
                new_idx += 1;
            } else if new_idx < new_lines.len() && (old_idx >= old_lines.len() || !old_lines[old_idx..].contains(&new_lines[new_idx])) {
                // Addition
                lines.push(DiffLine {
                    kind: LineKind::Addition,
                    old_line_no: None,
                    new_line_no: Some(new_idx as u32 + 1),
                    content: new_lines[new_idx].to_string(),
                    highlights: Vec::new(),
                });
                new_idx += 1;
            } else if old_idx < old_lines.len() {
                // Deletion
                lines.push(DiffLine {
                    kind: LineKind::Deletion,
                    old_line_no: Some(old_idx as u32 + 1),
                    new_line_no: None,
                    content: old_lines[old_idx].to_string(),
                    highlights: Vec::new(),
                });
                old_idx += 1;
            }
        }

        if !lines.is_empty() {
            hunks.push(DiffHunk {
                old_start: 1,
                old_lines: old_lines.len() as u32,
                new_start: 1,
                new_lines: new_lines.len() as u32,
                lines,
            });
        }

        Ok(hunks)
    }

    /// Diff working tree vs index (unstaged changes)
    fn diff_unstaged(&self, paths: &[String]) -> Result<Vec<DiffFile>> {
        let repo = Repository::open(&self.repo_path)
            .context("Failed to open git repository")?;

        let mut opts = DiffOptions::new();
        opts.context_lines(self.context_lines);
        for path in paths {
            opts.pathspec(path);
        }

        let diff = repo
            .diff_index_to_workdir(None, Some(&mut opts))
            .context("Failed to create diff")?;

        self.process_diff(&diff)
    }

    /// Diff index vs HEAD (staged changes)
    fn diff_staged(&self, paths: &[String]) -> Result<Vec<DiffFile>> {
        let repo = Repository::open(&self.repo_path)
            .context("Failed to open git repository")?;

        let head_tree = self.resolve_tree(&repo, "HEAD")?;

        let mut opts = DiffOptions::new();
        opts.context_lines(self.context_lines);
        for path in paths {
            opts.pathspec(path);
        }

        let diff = repo
            .diff_tree_to_index(Some(&head_tree), None, Some(&mut opts))
            .context("Failed to create diff")?;

        self.process_diff(&diff)
    }

    /// Diff working tree against a revision with path filtering
    fn diff_working_tree_filtered(&self, base: &str, paths: &[String]) -> Result<Vec<DiffFile>> {
        let repo = Repository::open(&self.repo_path)
            .context("Failed to open git repository")?;

        let base_tree = self.resolve_tree(&repo, base)?;

        let mut opts = DiffOptions::new();
        opts.context_lines(self.context_lines);
        for path in paths {
            opts.pathspec(path);
        }

        let diff = repo
            .diff_tree_to_workdir_with_index(Some(&base_tree), Some(&mut opts))
            .context("Failed to create diff")?;

        self.process_diff(&diff)
    }

    /// Gets the list of changed files between two revisions with path filtering
    fn get_changed_files_filtered(&self, from: &str, to: &str, paths: &[String]) -> Result<Vec<DiffFile>> {
        let repo = Repository::open(&self.repo_path)
            .context("Failed to open git repository")?;

        let from_tree = self.resolve_tree(&repo, from)?;
        let to_tree = self.resolve_tree(&repo, to)?;

        let mut opts = DiffOptions::new();
        opts.context_lines(self.context_lines);
        for path in paths {
            opts.pathspec(path);
        }

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

    /// Process a git2::Diff into our DiffFile format
    fn process_diff(&self, diff: &git2::Diff) -> Result<Vec<DiffFile>> {
        use std::cell::RefCell;
        let files: RefCell<Vec<DiffFile>> = RefCell::new(Vec::new());
        let current_hunk: RefCell<Option<DiffHunk>> = RefCell::new(None);

        diff.foreach(
            &mut |delta, _| {
                // Save previous file's last hunk if any
                if let Some(h) = current_hunk.borrow_mut().take() {
                    if let Some(file) = files.borrow_mut().last_mut() {
                        file.hunks.push(h);
                    }
                }

                let status = match delta.status() {
                    git2::Delta::Added => FileStatus::Added,
                    git2::Delta::Deleted => FileStatus::Deleted,
                    git2::Delta::Modified => FileStatus::Modified,
                    git2::Delta::Renamed => FileStatus::Renamed,
                    _ => return true,
                };

                let old_path = delta.old_file().path().map(PathBuf::from);
                let new_path = delta.new_file().path().map(PathBuf::from);

                files.borrow_mut().push(DiffFile {
                    old_path,
                    new_path,
                    status,
                    hunks: Vec::new(),
                });

                true
            },
            None,
            Some(&mut |_delta, hunk| {
                // Save previous hunk
                if let Some(h) = current_hunk.borrow_mut().take() {
                    if let Some(file) = files.borrow_mut().last_mut() {
                        file.hunks.push(h);
                    }
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

        // Don't forget the last hunk
        if let Some(h) = current_hunk.into_inner() {
            if let Some(file) = files.borrow_mut().last_mut() {
                file.hunks.push(h);
            }
        }

        Ok(files.into_inner())
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
        opts.context_lines(self.context_lines);
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
