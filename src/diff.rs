//! Diff engine using git diff command
//!
//! Uses `git diff` command directly to handle custom diff drivers properly.

use anyhow::{Context, Result};
use git2::Repository;
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

/// Diff engine using git diff command
pub struct DiffEngine {
    repo_path: PathBuf,
    context_lines: u32,
}

impl DiffEngine {
    pub fn new(repo_path: PathBuf, context_lines: u32) -> Self {
        Self {
            repo_path,
            context_lines,
        }
    }

    /// Main diff method - handles all diff modes
    pub fn diff(&self, mode: &DiffMode, paths: &[String]) -> Result<Vec<DiffFile>> {
        match mode {
            DiffMode::Unstaged => self.diff_via_git_cmd(&[], paths),
            DiffMode::Staged => self.diff_via_git_cmd(&["--staged"], paths),
            DiffMode::WorkingTree { base } => self.diff_via_git_cmd(&[base.as_str()], paths),
            DiffMode::Commits { from, to } => {
                let range = format!("{}..{}", from, to);
                self.diff_via_git_cmd(&[&range], paths)
            }
            DiffMode::MergeBase { from, to } => {
                let range = format!("{}...{}", from, to);
                self.diff_via_git_cmd(&[&range], paths)
            }
            DiffMode::ExternalDiff { path, old_file, new_file } => {
                self.diff_external_files(path, old_file, new_file)
            }
        }
    }

    /// Use git diff command directly - handles custom diff drivers properly
    fn diff_via_git_cmd(&self, args: &[&str], paths: &[String]) -> Result<Vec<DiffFile>> {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.repo_path)
            .arg("diff")
            .arg("--no-color")
            .arg(format!("-U{}", self.context_lines))
            .arg("--find-renames")
            .arg("--find-copies");

        for arg in args {
            cmd.arg(arg);
        }

        if !paths.is_empty() {
            cmd.arg("--");
            for path in paths {
                if !path.is_empty() {
                    cmd.arg(path);
                }
            }
        }

        let output = cmd.output().context("Failed to run git diff")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git diff failed: {}", stderr);
        }

        self.parse_full_diff(&String::from_utf8_lossy(&output.stdout))
    }

    /// Parse full unified diff output into DiffFile structs
    fn parse_full_diff(&self, diff_output: &str) -> Result<Vec<DiffFile>> {
        let mut files = Vec::new();
        let mut current_file: Option<DiffFile> = None;
        let mut current_hunk: Option<DiffHunk> = None;
        let mut old_line = 0u32;
        let mut new_line = 0u32;

        for line in diff_output.lines() {
            if line.starts_with("diff --git") {
                // Save previous file
                if let Some(mut f) = current_file.take() {
                    if let Some(h) = current_hunk.take() {
                        f.hunks.push(h);
                    }
                    files.push(f);
                }
                // Parse file paths from "diff --git a/path b/path"
                let paths = parse_diff_git_line(line);
                current_file = Some(DiffFile {
                    old_path: paths.0.map(PathBuf::from),
                    new_path: paths.1.map(PathBuf::from),
                    status: FileStatus::Modified, // Will be updated by index line
                    hunks: Vec::new(),
                });
            } else if line.starts_with("new file") {
                if let Some(ref mut f) = current_file {
                    f.status = FileStatus::Added;
                }
            } else if line.starts_with("deleted file") {
                if let Some(ref mut f) = current_file {
                    f.status = FileStatus::Deleted;
                }
            } else if line.starts_with("rename from") || line.starts_with("similarity index") {
                if let Some(ref mut f) = current_file {
                    f.status = FileStatus::Renamed;
                }
            } else if line.starts_with("@@") {
                // Save previous hunk
                if let Some(h) = current_hunk.take() {
                    if let Some(ref mut f) = current_file {
                        f.hunks.push(h);
                    }
                }

                if let Some(header) = parse_hunk_header(line) {
                    old_line = header.0;
                    new_line = header.2;
                    current_hunk = Some(DiffHunk {
                        old_start: header.0,
                        old_lines: header.1,
                        new_start: header.2,
                        new_lines: header.3,
                        lines: Vec::new(),
                    });
                }
            } else if let Some(ref mut hunk) = current_hunk {
                let (kind, old_no, new_no) = if line.starts_with('+') {
                    let no = new_line;
                    new_line += 1;
                    (LineKind::Addition, None, Some(no))
                } else if line.starts_with('-') {
                    let no = old_line;
                    old_line += 1;
                    (LineKind::Deletion, Some(no), None)
                } else if line.starts_with(' ') {
                    let old_no = old_line;
                    let new_no = new_line;
                    old_line += 1;
                    new_line += 1;
                    (LineKind::Context, Some(old_no), Some(new_no))
                } else if line.starts_with('\\') {
                    // "\ No newline at end of file"
                    continue;
                } else {
                    continue;
                };

                let content = if line.len() > 1 { &line[1..] } else { "" };
                hunk.lines.push(DiffLine {
                    kind,
                    old_line_no: old_no,
                    new_line_no: new_no,
                    content: content.to_string(),
                    highlights: Vec::new(),
                });
            }
        }

        // Don't forget the last file/hunk
        if let Some(mut f) = current_file {
            if let Some(h) = current_hunk {
                f.hunks.push(h);
            }
            files.push(f);
        }

        // Filter out files with no hunks
        files.retain(|f| !f.hunks.is_empty());

        Ok(files)
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

}

/// Parse "diff --git a/path b/path" line
fn parse_diff_git_line(line: &str) -> (Option<String>, Option<String>) {
    // "diff --git a/old/path b/new/path"
    let line = line.strip_prefix("diff --git ").unwrap_or(line);
    let parts: Vec<&str> = line.splitn(2, " b/").collect();

    let old_path = parts.first().and_then(|p| p.strip_prefix("a/")).map(String::from);
    let new_path = parts.get(1).map(|p| p.to_string());

    (old_path, new_path)
}

/// Parse a unified diff hunk header: @@ -old_start,old_count +new_start,new_count @@
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    // @@ -1,5 +1,7 @@
    let line = line.trim_start_matches('@').trim();
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }

    let old_part = parts[0].trim_start_matches('-');
    let new_part = parts[1].trim_start_matches('+');

    let (old_start, old_count) = parse_range(old_part)?;
    let (new_start, new_count) = parse_range(new_part)?;

    Some((old_start, old_count, new_start, new_count))
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    if let Some((start, count)) = s.split_once(',') {
        Some((start.parse().ok()?, count.parse().ok()?))
    } else {
        Some((s.parse().ok()?, 1))
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
