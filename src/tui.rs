//! TUI layer using ratatui and crossterm
//!
//! Provides interactive diff viewing with annotation support.

use crate::config::Config;
use crate::diff::{DiffEngine, DiffFile, DiffLine, HighlightRange, LineKind};
use crate::storage::{Annotation, AnnotationType, Side, Storage};
use crate::syntax::{SyntaxHighlighter, TokenType};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout};
use std::path::PathBuf;

const REATTACH_CONTEXT_LINES: usize = 2;
const REATTACH_WINDOW: i32 = 5;
const REATTACH_THRESHOLD: f32 = 0.7;

/// Represents a line in the unified display
#[derive(Debug, Clone)]
pub enum DisplayLine {
    /// Empty line for visual separation
    Spacer,
    /// File header with path
    FileHeader {
        path: String,
        #[allow(dead_code)]
        file_idx: usize,
    },
    /// Hunk separator showing line number and stats
    HunkHeader {
        line_no: u32,
        additions: usize,
        deletions: usize,
    },
    /// End of hunk marker for spacing
    HunkEnd,
    /// A diff line (addition, deletion, or context)
    Diff {
        line: DiffLine,
        #[allow(dead_code)]
        file_idx: usize,
        file_path: String,
    },
    /// An annotation shown inline below its line
    Annotation {
        annotation: Annotation,
        #[allow(dead_code)]
        file_idx: usize,
    },
}

/// Application state
#[allow(dead_code)]
pub struct App {
    storage: Storage,
    diff_engine: DiffEngine,
    repo_path: PathBuf,
    repo_id: i64,
    config: Config,

    // Diff state
    files: Vec<DiffFile>,
    display_lines: Vec<DisplayLine>,
    current_line_idx: usize,
    scroll_offset: usize,

    // All annotations keyed by (file_path, side, line_no)
    all_annotations: Vec<Annotation>,

    // Syntax highlighting
    syntax_highlighter: SyntaxHighlighter,

    // UI state
    mode: Mode,
    annotation_input: String,
    annotation_type: AnnotationType,
    message: Option<String>,
    show_help: bool,
    show_annotations: bool,
    side_by_side: bool,
    visible_height: usize,
    expanded_file: Option<usize>, // When Some, only show this file
    collapsed_files: HashSet<usize>,
    // Position to restore when collapsing expanded view
    pre_expand_position: Option<(usize, usize)>, // (line_idx, scroll_offset)
    annotation_list_idx: usize,

    // Search state
    search: Option<SearchState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Normal,
    AddAnnotation,
    EditAnnotation(i64), // annotation id
    SearchFile,          // searching by filename
    SearchContent,       // searching within content
    AnnotationList,
}

#[derive(Debug, Clone)]
struct SearchState {
    query: String,
    matches: Vec<usize>,      // indices into display_lines
    current_match: usize,     // index into matches
    search_type: SearchType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SearchType {
    File,
    Content,
}

#[derive(Clone, Copy)]
struct Theme {
    bg: Color,
    surface: Color,
    surface_alt: Color,
    current_line_bg: Color,
    header_bg: Color,
    header_fg: Color,
    header_match_bg: Color,
    header_match_fg: Color,
    header_dim_bg: Color,
    header_dim_fg: Color,
    header_focus_bg: Color,
    expanded_bg: Color,
    expanded_fg: Color,
    hunk_bg: Color,
    hunk_fg: Color,
    hunk_border: Color,
    added_bg: Color,
    added_fg: Color,
    deleted_bg: Color,
    deleted_fg: Color,
    context_fg: Color,
    line_num: Color,
    annotation_bg: Color,
    annotation_fg: Color,
    todo_bg: Color,
    todo_fg: Color,
    annotation_marker: Color,
    status_bg: Color,
    status_fg: Color,
    search_bg: Color,
    search_fg: Color,
    help_bg: Color,
    help_fg: Color,
    border: Color,
    token_keyword: Color,
    token_string: Color,
    token_comment: Color,
    token_function: Color,
    token_type: Color,
    token_number: Color,
    token_operator: Color,
    token_variable: Color,
    token_atom: Color,
    token_module: Color,
    token_default: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            bg: Color::Rgb(18, 20, 24),
            surface: Color::Rgb(26, 30, 36),
            surface_alt: Color::Rgb(34, 38, 46),
            current_line_bg: Color::Rgb(64, 72, 86),
            header_bg: Color::Rgb(38, 44, 60),
            header_fg: Color::Rgb(235, 238, 242),
            header_match_bg: Color::Rgb(220, 190, 90),
            header_match_fg: Color::Rgb(18, 18, 18),
            header_dim_bg: Color::Rgb(120, 120, 70),
            header_dim_fg: Color::Rgb(20, 20, 20),
            header_focus_bg: Color::Rgb(58, 66, 84),
            expanded_bg: Color::Rgb(96, 72, 40),
            expanded_fg: Color::Rgb(255, 245, 230),
            hunk_bg: Color::Rgb(140, 130, 78),
            hunk_fg: Color::Rgb(20, 20, 20),
            hunk_border: Color::Rgb(80, 88, 72),
            added_bg: Color::Rgb(18, 44, 26),
            added_fg: Color::Rgb(150, 230, 185),
            deleted_bg: Color::Rgb(52, 24, 26),
            deleted_fg: Color::Rgb(240, 160, 160),
            context_fg: Color::Rgb(220, 224, 230),
            line_num: Color::Rgb(120, 130, 140),
            annotation_bg: Color::Rgb(40, 76, 78),
            annotation_fg: Color::Rgb(230, 245, 245),
            todo_bg: Color::Rgb(115, 86, 26),
            todo_fg: Color::Rgb(255, 245, 210),
            annotation_marker: Color::Rgb(255, 208, 96),
            status_bg: Color::Rgb(22, 24, 28),
            status_fg: Color::Rgb(150, 160, 170),
            search_bg: Color::Rgb(40, 44, 56),
            search_fg: Color::Rgb(255, 225, 140),
            help_bg: Color::Rgb(30, 34, 42),
            help_fg: Color::Rgb(220, 230, 240),
            border: Color::Rgb(210, 175, 90),
            token_keyword: Color::Rgb(205, 140, 255),
            token_string: Color::Rgb(255, 206, 120),
            token_comment: Color::Rgb(120, 130, 140),
            token_function: Color::Rgb(120, 190, 255),
            token_type: Color::Rgb(120, 220, 210),
            token_number: Color::Rgb(240, 170, 170),
            token_operator: Color::Rgb(220, 224, 230),
            token_variable: Color::Rgb(150, 210, 255),
            token_atom: Color::Rgb(130, 220, 210),
            token_module: Color::Rgb(240, 215, 140),
            token_default: Color::Rgb(220, 224, 230),
        }
    }
}

impl App {
    pub fn new(
        storage: Storage,
        diff_engine: DiffEngine,
        repo_path: PathBuf,
        repo_id: i64,
        files: Vec<DiffFile>,
        config: Config,
    ) -> Self {
        let show_annotations = config.show_annotations;
        let side_by_side = config.side_by_side;

        Self {
            storage,
            diff_engine,
            repo_path,
            repo_id,
            config,
            files,
            display_lines: Vec::new(),
            current_line_idx: 0,
            scroll_offset: 0,
            all_annotations: Vec::new(),
            syntax_highlighter: SyntaxHighlighter::new(),
            mode: Mode::Normal,
            annotation_input: String::new(),
            annotation_type: AnnotationType::Comment,
            message: None,
            show_help: false,
            show_annotations,
            side_by_side,
            visible_height: 20, // Will be updated on first render
            expanded_file: None,
            collapsed_files: HashSet::new(),
            pre_expand_position: None,
            search: None,
            annotation_list_idx: 0,
        }
    }

    /// Load all annotations for all files and build the display lines
    fn load_all_annotations(&mut self) -> Result<()> {
        self.all_annotations.clear();

        let mut file_cache: HashMap<String, Vec<String>> = HashMap::new();

        let file_paths: Vec<String> = self
            .files
            .iter()
            .filter_map(|file| {
                file.new_path
                    .as_ref()
                    .or(file.old_path.as_ref())
                    .map(|p| p.to_string_lossy().to_string())
            })
            .collect();

        for path in file_paths {
            let mut annotations = self.storage.list_annotations(self.repo_id, Some(&path))?;
            if annotations.is_empty() {
                continue;
            }
            if let Some(lines) = self.read_file_lines(&path, &mut file_cache) {
                self.reattach_annotations_for_file(&mut annotations, &lines)?;
            }
            self.all_annotations.extend(annotations);
        }

        Ok(())
    }

    fn read_file_lines(
        &self,
        file_path: &str,
        cache: &mut HashMap<String, Vec<String>>,
    ) -> Option<Vec<String>> {
        if let Some(lines) = cache.get(file_path) {
            return Some(lines.clone());
        }
        let full_path = self.repo_path.join(file_path);
        let content = std::fs::read_to_string(&full_path).ok()?;
        let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
        cache.insert(file_path.to_string(), lines.clone());
        Some(lines)
    }

    fn reattach_annotations_for_file(
        &mut self,
        annotations: &mut [Annotation],
        lines: &[String],
    ) -> Result<()> {
        for annotation in annotations.iter_mut() {
            if annotation.side != Side::New {
                continue;
            }

            let anchor_line = if annotation.anchor_line > 0 {
                annotation.anchor_line
            } else {
                annotation.start_line
            };

            if annotation.anchor_text.is_empty() {
                let (anchor_line, anchor_text, context_before, context_after) =
                    Self::build_anchor(lines, annotation.start_line);
                if !anchor_text.is_empty() {
                    if let Err(err) = self.storage.update_annotation_location(
                        annotation.id,
                        annotation.start_line,
                        annotation.end_line,
                        anchor_line,
                        &anchor_text,
                        &context_before,
                        &context_after,
                    ) {
                        self.message = Some(format!("Anchor update failed: {}", err));
                    }
                    annotation.anchor_line = anchor_line;
                    annotation.anchor_text = anchor_text;
                    annotation.context_before = context_before;
                    annotation.context_after = context_after;
                }
                continue;
            }

            if lines.is_empty() {
                continue;
            }

            let base = anchor_line as i32;
            let start = (base - REATTACH_WINDOW).max(1) as usize;
            let end = (base + REATTACH_WINDOW).min(lines.len() as i32) as usize;

            let mut best_line = anchor_line;
            let mut best_score = -1.0f32;

            for line_no in start..=end {
                let candidate = lines.get(line_no - 1).map(String::as_str).unwrap_or("");
                let line_score = Self::similarity(annotation.anchor_text.as_str(), candidate);
                let ctx_bonus = Self::context_bonus(lines, line_no, &annotation.context_before, &annotation.context_after);
                let score = line_score + ctx_bonus;
                if score > best_score {
                    best_score = score;
                    best_line = line_no as u32;
                }
            }

            if best_score >= REATTACH_THRESHOLD {
                let range_len = annotation
                    .end_line
                    .map(|end| end.saturating_sub(annotation.start_line));
                let new_end = range_len.map(|len| best_line.saturating_add(len));
                let (anchor_line, anchor_text, context_before, context_after) =
                    Self::build_anchor(lines, best_line);

                if let Err(err) = self.storage.update_annotation_location(
                    annotation.id,
                    best_line,
                    new_end,
                    anchor_line,
                    &anchor_text,
                    &context_before,
                    &context_after,
                ) {
                    self.message = Some(format!("Reattach update failed: {}", err));
                }
                annotation.start_line = best_line;
                annotation.end_line = new_end;
                annotation.anchor_line = anchor_line;
                annotation.anchor_text = anchor_text;
                annotation.context_before = context_before;
                annotation.context_after = context_after;
            }
        }

        Ok(())
    }

    fn build_anchor(lines: &[String], line_no: u32) -> (u32, String, String, String) {
        if line_no == 0 || lines.is_empty() {
            return (line_no, String::new(), String::new(), String::new());
        }
        let idx = line_no.saturating_sub(1) as usize;
        if idx >= lines.len() {
            return (line_no, String::new(), String::new(), String::new());
        }
        let anchor_text = lines.get(idx).cloned().unwrap_or_default();
        let start = idx.saturating_sub(REATTACH_CONTEXT_LINES);
        let before = lines[start..idx].join("\n");
        let after_end = (idx + 1 + REATTACH_CONTEXT_LINES).min(lines.len());
        let after = if idx + 1 < after_end {
            lines[idx + 1..after_end].join("\n")
        } else {
            String::new()
        };
        (line_no, anchor_text, before, after)
    }

    fn context_bonus(lines: &[String], line_no: usize, before: &str, after: &str) -> f32 {
        let mut bonus = 0.0;
        if !before.is_empty() {
            let ctx: Vec<&str> = before.split('\n').collect();
            for (i, ctx_line) in ctx.iter().rev().enumerate() {
                let idx = line_no.saturating_sub(2 + i);
                if idx < lines.len() && lines[idx].trim() == ctx_line.trim() {
                    bonus += 0.1;
                }
            }
        }
        if !after.is_empty() {
            let ctx: Vec<&str> = after.split('\n').collect();
            for (i, ctx_line) in ctx.iter().enumerate() {
                let idx = line_no + i;
                if idx < lines.len() && lines[idx].trim() == ctx_line.trim() {
                    bonus += 0.1;
                }
            }
        }
        bonus
    }

    fn similarity(a: &str, b: &str) -> f32 {
        let a = a.trim();
        let b = b.trim();
        if a.is_empty() && b.is_empty() {
            return 1.0;
        }
        if a.is_empty() || b.is_empty() {
            return 0.0;
        }
        let dist = Self::levenshtein(a.as_bytes(), b.as_bytes()) as f32;
        let max_len = a.len().max(b.len()) as f32;
        if max_len == 0.0 {
            1.0
        } else {
            1.0 - (dist / max_len)
        }
    }

    fn levenshtein(a: &[u8], b: &[u8]) -> usize {
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        let mut curr = vec![0; b.len() + 1];

        for (i, &ac) in a.iter().enumerate() {
            curr[0] = i + 1;
            for (j, &bc) in b.iter().enumerate() {
                let cost = if ac == bc { 0 } else { 1 };
                curr[j + 1] = std::cmp::min(
                    std::cmp::min(curr[j] + 1, prev[j + 1] + 1),
                    prev[j] + cost,
                );
            }
            prev.clone_from_slice(&curr);
        }

        prev[b.len()]
    }

    /// Build the unified display lines from all files
    fn build_display_lines(&mut self) {
        self.display_lines.clear();

        // Clone files to avoid borrow issues
        let files = self.files.clone();
        let expanded_file = self.expanded_file;

        for (file_idx, file) in files.iter().enumerate() {
            // Skip files if we're in expanded mode for a different file
            if let Some(expanded_idx) = expanded_file {
                if file_idx != expanded_idx {
                    continue;
                }
            }

            let file_path = file
                .new_path
                .as_ref()
                .or(file.old_path.as_ref())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "<unknown>".to_string());

            // Add spacing before file header (except first file or expanded mode)
            if file_idx > 0 && expanded_file.is_none() {
                self.display_lines.push(DisplayLine::Spacer);
                self.display_lines.push(DisplayLine::Spacer);
            }

            // Add file header
            self.display_lines.push(DisplayLine::FileHeader {
                path: file_path.clone(),
                file_idx,
            });

            if self.collapsed_files.contains(&file_idx) && expanded_file.is_none() {
                continue;
            }

            // If expanded, show full file with changes highlighted
            if expanded_file.is_some() {
                self.build_expanded_file_lines(file_idx, &file, &file_path);
            } else {
                self.build_diff_hunk_lines(file_idx, &file, &file_path);
            }
        }
    }

    /// Build display lines for diff hunks (normal mode)
    fn build_diff_hunk_lines(&mut self, file_idx: usize, file: &DiffFile, file_path: &str) {
        for hunk in file.hunks.iter() {
            // Count additions and deletions in this hunk
            let additions = hunk.lines.iter().filter(|l| l.kind == LineKind::Addition).count();
            let deletions = hunk.lines.iter().filter(|l| l.kind == LineKind::Deletion).count();

            // Add spacing before hunk
            self.display_lines.push(DisplayLine::Spacer);

            // Add hunk header
            let line_no = hunk.new_start;
            self.display_lines.push(DisplayLine::HunkHeader {
                line_no,
                additions,
                deletions,
            });

            for line in &hunk.lines {
                let mut highlighted_line = line.clone();
                self.highlight_line(&mut highlighted_line, file_path);

                self.display_lines.push(DisplayLine::Diff {
                    line: highlighted_line.clone(),
                    file_idx,
                    file_path: file_path.to_string(),
                });

                self.add_annotations_for_line(file_idx, file_path, &highlighted_line);
            }

            // Add hunk end marker
            self.display_lines.push(DisplayLine::HunkEnd);
        }
    }

    /// Build display lines for full file view (expanded mode)
    fn build_expanded_file_lines(&mut self, file_idx: usize, file: &DiffFile, file_path: &str) {
        use std::collections::HashMap;

        // Build maps of line changes from diff hunks
        // Key: new_line_no for additions/context, old_line_no for deletions
        let mut additions: HashMap<u32, String> = HashMap::new();
        let mut deletions: Vec<(u32, String)> = Vec::new(); // (insert_after_new_line, content)

        for hunk in &file.hunks {
            let mut last_new_line: u32 = hunk.new_start.saturating_sub(1);
            for line in &hunk.lines {
                match line.kind {
                    LineKind::Addition => {
                        if let Some(n) = line.new_line_no {
                            additions.insert(n, line.content.clone());
                            last_new_line = n;
                        }
                    }
                    LineKind::Deletion => {
                        deletions.push((last_new_line, line.content.clone()));
                    }
                    LineKind::Context => {
                        if let Some(n) = line.new_line_no {
                            last_new_line = n;
                        }
                    }
                }
            }
        }

        // Read the full file from disk
        let full_path = self.repo_path.join(file_path);
        let file_content = std::fs::read_to_string(&full_path).unwrap_or_default();
        let file_lines: Vec<&str> = file_content.lines().collect();

        // Show all lines with changes highlighted
        for (idx, content) in file_lines.iter().enumerate() {
            let line_no = (idx + 1) as u32;

            // First, show any deletions that come before this line
            for (insert_after, del_content) in &deletions {
                if *insert_after == line_no.saturating_sub(1) {
                    let mut del_line = DiffLine {
                        kind: LineKind::Deletion,
                        old_line_no: None,
                        new_line_no: None,
                        content: del_content.clone(),
                        highlights: Vec::new(),
                    };
                    self.highlight_line(&mut del_line, file_path);
                    self.display_lines.push(DisplayLine::Diff {
                        line: del_line,
                        file_idx,
                        file_path: file_path.to_string(),
                    });
                }
            }

            // Determine if this line is an addition
            let kind = if additions.contains_key(&line_no) {
                LineKind::Addition
            } else {
                LineKind::Context
            };

            let mut diff_line = DiffLine {
                kind,
                old_line_no: Some(line_no),
                new_line_no: Some(line_no),
                content: content.to_string(),
                highlights: Vec::new(),
            };
            self.highlight_line(&mut diff_line, file_path);

            self.display_lines.push(DisplayLine::Diff {
                line: diff_line.clone(),
                file_idx,
                file_path: file_path.to_string(),
            });

            self.add_annotations_for_line(file_idx, file_path, &diff_line);
        }

        // Show any trailing deletions
        let last_line = file_lines.len() as u32;
        for (insert_after, del_content) in &deletions {
            if *insert_after >= last_line {
                let mut del_line = DiffLine {
                    kind: LineKind::Deletion,
                    old_line_no: None,
                    new_line_no: None,
                    content: del_content.clone(),
                    highlights: Vec::new(),
                };
                self.highlight_line(&mut del_line, file_path);
                self.display_lines.push(DisplayLine::Diff {
                    line: del_line,
                    file_idx,
                    file_path: file_path.to_string(),
                });
            }
        }
    }

    /// Add annotations for a diff line
    fn add_annotations_for_line(&mut self, file_idx: usize, file_path: &str, line: &DiffLine) {
        if !self.show_annotations {
            return;
        }

        let line_no = line.new_line_no.or(line.old_line_no).unwrap_or(0);
        let side = if line.new_line_no.is_some() {
            Side::New
        } else {
            Side::Old
        };

        for annotation in &self.all_annotations {
            if annotation.file_path == file_path
                && annotation.side == side
                && annotation.start_line <= line_no
                && annotation.end_line.map_or(annotation.start_line == line_no, |e| e >= line_no)
            {
                self.display_lines.push(DisplayLine::Annotation {
                    annotation: annotation.clone(),
                    file_idx,
                });
            }
        }
    }

    /// Add syntax highlighting to a diff line
    fn highlight_line(&mut self, line: &mut DiffLine, file_path: &str) {
        if !self.config.syntax_highlighting {
            return;
        }

        let syntax_highlights = self.syntax_highlighter.highlight_for_file(&line.content, file_path);

        line.highlights = syntax_highlights
            .into_iter()
            .map(|h| HighlightRange {
                start: h.start,
                end: h.end,
                token_type: h.token_type,
            })
            .collect();
    }

    /// Get the current display line
    fn current_display_line(&self) -> Option<&DisplayLine> {
        self.display_lines.get(self.current_line_idx)
    }

    /// Get the current file info (path and index) based on current position
    fn current_file_info(&self) -> Option<(String, usize)> {
        // Search backwards to find the most recent file header
        for i in (0..=self.current_line_idx).rev() {
            if let Some(DisplayLine::FileHeader { path, file_idx }) = self.display_lines.get(i) {
                return Some((path.clone(), *file_idx));
            }
        }
        None
    }

    /// Find the index of the current file's header in display_lines
    fn find_current_file_header_idx(&self) -> Option<usize> {
        for i in (0..=self.current_line_idx).rev() {
            if matches!(self.display_lines.get(i), Some(DisplayLine::FileHeader { .. })) {
                return Some(i);
            }
        }
        None
    }

    /// Check if a file is a search match (for file search)
    fn is_file_search_match(&self, file_idx: usize) -> bool {
        let Some(ref search) = self.search else {
            return false;
        };

        // Only highlight for file search type
        if search.search_type != SearchType::File {
            return false;
        }

        // Check if any match corresponds to this file's header
        for &match_idx in &search.matches {
            if let Some(DisplayLine::FileHeader { file_idx: idx, .. }) = self.display_lines.get(match_idx) {
                if *idx == file_idx {
                    return true;
                }
            }
        }
        false
    }

    /// Check if a display line index is a search match
    fn is_search_match(&self, idx: usize) -> bool {
        self.search.as_ref().map(|s| s.matches.contains(&idx)).unwrap_or(false)
    }

    /// Check if a display line index is the current search match
    fn is_current_search_match(&self, idx: usize) -> bool {
        self.search.as_ref().map(|s| {
            s.matches.get(s.current_match) == Some(&idx)
        }).unwrap_or(false)
    }

    /// Get annotation for the current diff line
    fn get_annotation_for_current_line(&self) -> Option<&Annotation> {
        if let Some(DisplayLine::Diff { line, file_path, .. }) = self.current_display_line() {
            let line_no = line.new_line_no.or(line.old_line_no).unwrap_or(0);
            let side = if line.new_line_no.is_some() {
                Side::New
            } else {
                Side::Old
            };

            return self.all_annotations.iter().find(|a| {
                a.file_path == *file_path
                    && a.side == side
                    && a.start_line <= line_no
                    && a.end_line.map_or(a.start_line == line_no, |e| e >= line_no)
            });
        }
        None
    }

    fn add_annotation(&mut self) -> Result<()> {
        if let Some(DisplayLine::Diff { line, file_path, .. }) = self.current_display_line().cloned() {
            let line_no = line.new_line_no.or(line.old_line_no).unwrap_or(1);
            let side = if line.new_line_no.is_some() {
                Side::New
            } else {
                Side::Old
            };

            let (anchor_line, anchor_text, context_before, context_after) = if side == Side::New {
                let mut cache = HashMap::new();
                if let Some(lines) = self.read_file_lines(&file_path, &mut cache) {
                    Self::build_anchor(&lines, line_no)
                } else {
                    (line_no, line.content.clone(), String::new(), String::new())
                }
            } else {
                (line_no, line.content.clone(), String::new(), String::new())
            };

            self.storage.add_annotation(
                self.repo_id,
                &file_path,
                None, // commit_sha
                side,
                line_no,
                None, // end_line
                self.annotation_type.clone(),
                &self.annotation_input,
                anchor_line,
                &anchor_text,
                &context_before,
                &context_after,
            )?;

            self.message = Some("Annotation added".to_string());
            self.load_all_annotations()?;
            self.build_display_lines();
        }

        self.annotation_input.clear();
        self.mode = Mode::Normal;
        Ok(())
    }

    fn edit_annotation(&mut self, id: i64) -> Result<()> {
        self.storage
            .update_annotation(id, &self.annotation_input, self.annotation_type.clone())?;
        self.message = Some("Annotation updated".to_string());
        self.load_all_annotations()?;
        self.build_display_lines();
        self.annotation_input.clear();
        self.mode = Mode::Normal;
        Ok(())
    }

    fn delete_annotation_at_line(&mut self) -> Result<()> {
        if let Some(annotation) = self.get_annotation_for_current_line() {
            let id = annotation.id;
            self.storage.delete_annotation(id)?;
            self.message = Some("Annotation deleted".to_string());
            self.load_all_annotations()?;
            self.build_display_lines();
        }
        Ok(())
    }

    /// Check if a display line should be skipped during navigation
    fn is_skippable_line(&self, idx: usize) -> bool {
        match self.display_lines.get(idx) {
            Some(DisplayLine::FileHeader { file_idx, .. }) => !self.collapsed_files.contains(file_idx),
            Some(DisplayLine::Annotation { .. })
            | Some(DisplayLine::HunkHeader { .. })
            | Some(DisplayLine::HunkEnd)
            | Some(DisplayLine::Spacer) => true,
            _ => false,
        }
    }

    fn navigate_up(&mut self) {
        if self.current_line_idx > 0 {
            self.current_line_idx -= 1;
            // Skip over non-navigable lines
            while self.current_line_idx > 0 && self.is_skippable_line(self.current_line_idx) {
                self.current_line_idx -= 1;
            }
            self.ensure_cursor_on_navigable();
            self.adjust_scroll();
        }
    }

    fn navigate_down(&mut self) {
        let total_lines = self.display_lines.len();
        if self.current_line_idx < total_lines.saturating_sub(1) {
            self.current_line_idx += 1;
            // Skip over non-navigable lines
            while self.current_line_idx < total_lines.saturating_sub(1)
                && self.is_skippable_line(self.current_line_idx)
            {
                self.current_line_idx += 1;
            }
            self.ensure_cursor_on_navigable();
            self.adjust_scroll();
        }
    }

    /// Move down by half a page (Ctrl+D)
    fn page_down(&mut self) {
        let half_page = self.visible_height / 2;
        let total_lines = self.display_lines.len();

        for _ in 0..half_page {
            if self.current_line_idx < total_lines.saturating_sub(1) {
                self.current_line_idx += 1;
                // Skip non-navigable lines
                while self.current_line_idx < total_lines.saturating_sub(1)
                    && self.is_skippable_line(self.current_line_idx)
                {
                    self.current_line_idx += 1;
                }
            }
        }
        self.ensure_cursor_on_navigable();
        self.adjust_scroll();
    }

    /// Move up by half a page (Ctrl+U)
    fn page_up(&mut self) {
        let half_page = self.visible_height / 2;

        for _ in 0..half_page {
            if self.current_line_idx > 0 {
                self.current_line_idx -= 1;
                // Skip non-navigable lines
                while self.current_line_idx > 0 && self.is_skippable_line(self.current_line_idx) {
                    self.current_line_idx -= 1;
                }
            }
        }
        self.ensure_cursor_on_navigable();
        self.adjust_scroll();
    }

    /// Jump to next file header
    fn next_file(&mut self) {
        let current = self.current_line_idx;
        for (idx, line) in self.display_lines.iter().enumerate().skip(current + 1) {
            if let DisplayLine::FileHeader { .. } = line {
                self.current_line_idx = idx;
                // Skip to first diff line after header
                self.navigate_down();
                self.adjust_scroll();
                return;
            }
        }
    }

    /// Jump to previous file header
    fn prev_file(&mut self) {
        // First, find the current file's header
        let mut current_file_header = 0;
        for i in (0..=self.current_line_idx).rev() {
            if let Some(DisplayLine::FileHeader { .. }) = self.display_lines.get(i) {
                current_file_header = i;
                break;
            }
        }

        // Now find the previous file header before that
        if current_file_header > 0 {
            for i in (0..current_file_header).rev() {
                if let Some(DisplayLine::FileHeader { .. }) = self.display_lines.get(i) {
                    self.current_line_idx = i;
                    // Skip to first diff line after header
                    self.navigate_down();
                    self.adjust_scroll();
                    return;
                }
            }
        }
    }

    fn handle_input(&mut self, key: KeyEvent) -> Result<bool> {
        // Clear message on any input
        self.message = None;

        match &self.mode {
            Mode::Normal => self.handle_normal_input(key),
            Mode::AddAnnotation | Mode::EditAnnotation(_) => self.handle_annotation_input(key),
            Mode::SearchFile | Mode::SearchContent => self.handle_search_input(key),
            Mode::AnnotationList => self.handle_annotation_list_input(key),
        }
    }

    fn handle_normal_input(&mut self, key: KeyEvent) -> Result<bool> {
        // Handle Ctrl+key combinations first
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('d') => {
                    self.page_down();
                    return Ok(false);
                }
                KeyCode::Char('u') => {
                    self.page_up();
                    return Ok(false);
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Char('q') => return Ok(true), // Quit
            KeyCode::Char('?') => self.show_help = !self.show_help,

            // Navigation
            KeyCode::Char('j') | KeyCode::Down => self.navigate_down(),
            KeyCode::Char('k') | KeyCode::Up => self.navigate_up(),
            // n/N: next/prev search match if searching, otherwise next/prev chunk
            KeyCode::Char('n') => {
                if self.search.is_some() {
                    self.next_search_match();
                } else {
                    self.next_change_chunk();
                }
            }
            KeyCode::Char('N') => {
                if self.search.is_some() {
                    self.prev_search_match();
                } else {
                    self.prev_change_chunk();
                }
            }
            // Tab/Shift+Tab: next/prev file
            KeyCode::Tab => self.next_file(),
            KeyCode::BackTab => self.prev_file(),
            // Search
            KeyCode::Char('f') => {
                self.mode = Mode::SearchFile;
                self.search = Some(SearchState {
                    query: String::new(),
                    matches: Vec::new(),
                    current_match: 0,
                    search_type: SearchType::File,
                });
            }
            KeyCode::Char('/') => {
                self.mode = Mode::SearchContent;
                self.search = Some(SearchState {
                    query: String::new(),
                    matches: Vec::new(),
                    current_match: 0,
                    search_type: SearchType::Content,
                });
            }
            // Clear search
            KeyCode::Esc => {
                if self.show_help {
                    self.show_help = false;
                } else {
                    self.search = None;
                }
            }
            KeyCode::Char('g') => {
                self.current_line_idx = 0;
                // Skip to first navigable line
                while self.current_line_idx < self.display_lines.len().saturating_sub(1)
                    && self.is_skippable_line(self.current_line_idx)
                {
                    self.current_line_idx += 1;
                }
                self.scroll_offset = 0;
            }
            KeyCode::Char('G') => {
                let total = self.display_lines.len();
                self.current_line_idx = total.saturating_sub(1);
                // Skip to last navigable line
                while self.current_line_idx > 0 && self.is_skippable_line(self.current_line_idx) {
                    self.current_line_idx -= 1;
                }
                self.adjust_scroll();
            }


            // Toggle views
            KeyCode::Char('c') => {
                self.toggle_collapse_current_file();
            }
            KeyCode::Char('A') => {
                self.mode = Mode::AnnotationList;
                self.annotation_list_idx = 0;
            }
            KeyCode::Char('s') => {
                self.side_by_side = !self.side_by_side;
                self.message = Some(format!(
                    "View: {}",
                    if self.side_by_side { "side-by-side" } else { "unified" }
                ));
            }
            KeyCode::Char('x') => {
                // Toggle expanded/focused file view
                if self.expanded_file.is_some() {
                    // Collapse back to all files - restore previous position
                    self.expanded_file = None;
                    self.build_display_lines();

                    if let Some((saved_line_idx, saved_scroll)) = self.pre_expand_position.take() {
                        // Restore position, clamping to valid range
                        self.current_line_idx = saved_line_idx.min(self.display_lines.len().saturating_sub(1));
                        self.scroll_offset = saved_scroll;
                        // Skip to nearest navigable line if needed
                        while self.current_line_idx < self.display_lines.len().saturating_sub(1)
                            && self.is_skippable_line(self.current_line_idx)
                        {
                            self.current_line_idx += 1;
                        }
                    } else {
                        self.current_line_idx = 0;
                        self.scroll_offset = 0;
                        while self.current_line_idx < self.display_lines.len().saturating_sub(1)
                            && self.is_skippable_line(self.current_line_idx)
                        {
                            self.current_line_idx += 1;
                        }
                    }
                    self.adjust_scroll();
                    self.message = Some("Showing all files".to_string());
                } else {
                    // Expand current file - save position first
                    if let Some((_, file_idx)) = self.current_file_info() {
                        self.pre_expand_position = Some((self.current_line_idx, self.scroll_offset));
                        self.expanded_file = Some(file_idx);
                        self.build_display_lines();
                        self.current_line_idx = 0;
                        self.scroll_offset = 0;
                        // Skip to first navigable line
                        while self.current_line_idx < self.display_lines.len().saturating_sub(1)
                            && self.is_skippable_line(self.current_line_idx)
                        {
                            self.current_line_idx += 1;
                        }
                        self.message = Some("Expanded file view (]/[: next/prev change, x: collapse)".to_string());
                    }
                }
            }

            // Annotations
            KeyCode::Char('a') => {
                // Only allow adding annotations on diff lines
                if let Some(DisplayLine::Diff { .. }) = self.current_display_line() {
                    self.mode = Mode::AddAnnotation;
                    self.annotation_input.clear();
                } else {
                    self.message = Some("Move to a diff line to add annotation".to_string());
                }
            }
            KeyCode::Char('e') => {
                if let Some((id, content, a_type)) = self
                    .get_annotation_for_current_line()
                    .map(|a| (a.id, a.content.clone(), a.annotation_type.clone()))
                {
                    self.annotation_input = content;
                    self.annotation_type = a_type;
                    self.mode = Mode::EditAnnotation(id);
                }
            }
            KeyCode::Char('d') => self.delete_annotation_at_line()?,

            // Annotation type toggle
            KeyCode::Char('t') => {
                if let Some(annotation) = self.get_annotation_for_current_line() {
                    let id = annotation.id;
                    let content = annotation.content.clone();
                    let new_type = match annotation.annotation_type {
                        AnnotationType::Comment => AnnotationType::Todo,
                        AnnotationType::Todo => AnnotationType::Comment,
                    };
                    self.storage.update_annotation(id, &content, new_type.clone())?;
                    self.message = Some(format!("Annotation type: {}", new_type.as_str()));
                    self.load_all_annotations()?;
                    self.build_display_lines();
                } else {
                    self.annotation_type = match self.annotation_type {
                        AnnotationType::Comment => AnnotationType::Todo,
                        AnnotationType::Todo => AnnotationType::Comment,
                    };
                    self.message = Some(format!(
                        "Annotation type: {}",
                        self.annotation_type.as_str()
                    ));
                }
            }

            _ => {}
        }

        Ok(false)
    }

    fn handle_annotation_input(&mut self, key: KeyEvent) -> Result<bool> {
        // Ctrl+J inserts newline
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('j') {
            self.annotation_input.push('\n');
            return Ok(false);
        }
        // Ctrl+T toggles annotation type while editing/adding
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
            self.annotation_type = match self.annotation_type {
                AnnotationType::Comment => AnnotationType::Todo,
                AnnotationType::Todo => AnnotationType::Comment,
            };
            return Ok(false);
        }

        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.annotation_input.clear();
            }
            KeyCode::Enter => {
                // Plain Enter (no modifiers) saves
                if !self.annotation_input.is_empty() {
                    match &self.mode {
                        Mode::AddAnnotation => self.add_annotation()?,
                        Mode::EditAnnotation(id) => {
                            let id = *id;
                            self.edit_annotation(id)?;
                        }
                        _ => {}
                    }
                }
            }
            KeyCode::Backspace => {
                self.annotation_input.pop();
            }
            KeyCode::Char(c) => {
                self.annotation_input.push(c);
            }
            _ => {}
        }

        Ok(false)
    }

    fn handle_search_input(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.search = None;
            }
            KeyCode::Enter => {
                // Execute search and go to first match
                self.execute_search();
                self.mode = Mode::Normal;
                if let Some(ref search) = self.search {
                    if search.matches.is_empty() {
                        self.message = Some("No matches found".to_string());
                    } else {
                        self.message = Some(format!(
                            "Found {} match{}",
                            search.matches.len(),
                            if search.matches.len() == 1 { "" } else { "es" }
                        ));
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(ref mut search) = self.search {
                    search.query.pop();
                    // Live search as you type
                    self.execute_search();
                }
            }
            KeyCode::Char(c) => {
                if let Some(ref mut search) = self.search {
                    search.query.push(c);
                    // Live search as you type
                    self.execute_search();
                }
            }
            _ => {}
        }

        Ok(false)
    }

    fn handle_annotation_list_input(&mut self, key: KeyEvent) -> Result<bool> {
        let entries = self.annotation_list_entries();
        if entries.is_empty() {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                self.mode = Mode::Normal;
            }
            return Ok(false);
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Normal;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.annotation_list_idx =
                    (self.annotation_list_idx + 1).min(entries.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.annotation_list_idx = self.annotation_list_idx.saturating_sub(1);
            }
            KeyCode::Char('d') => {
                if let Some(entry) = entries.get(self.annotation_list_idx) {
                    self.storage.delete_annotation(entry.id)?;
                    self.load_all_annotations()?;
                    self.build_display_lines();
                    self.annotation_list_idx =
                        self.annotation_list_idx.min(self.annotation_list_entries().len().saturating_sub(1));
                }
            }
            KeyCode::Enter => {
                if let Some(entry) = entries.get(self.annotation_list_idx) {
                    if let Some(idx) = entry.display_idx {
                        self.current_line_idx = idx;
                        self.ensure_cursor_on_navigable();
                        self.adjust_scroll();
                        self.mode = Mode::Normal;
                    } else {
                        self.message = Some("Annotation not present in current diff".to_string());
                    }
                }
            }
            KeyCode::Char('e') => {
                if let Some(entry) = entries.get(self.annotation_list_idx) {
                    self.annotation_input = entry.content.clone();
                    self.annotation_type = entry.annotation_type.clone();
                    self.mode = Mode::EditAnnotation(entry.id);
                }
            }
            _ => {}
        }

        Ok(false)
    }

    fn execute_search(&mut self) {
        let Some(ref mut search) = self.search else {
            return;
        };

        if search.query.is_empty() {
            search.matches.clear();
            return;
        }

        // Try to compile as case-insensitive regex, fall back to literal match
        let pattern = regex::RegexBuilder::new(&search.query)
            .case_insensitive(true)
            .build()
            .unwrap_or_else(|_| {
                regex::RegexBuilder::new(&regex::escape(&search.query))
                    .case_insensitive(true)
                    .build()
                    .unwrap()
            });

        search.matches.clear();

        for (idx, line) in self.display_lines.iter().enumerate() {
            let matches = match search.search_type {
                SearchType::File => {
                    // Match on file headers
                    if let DisplayLine::FileHeader { path, .. } = line {
                        pattern.is_match(path)
                    } else {
                        false
                    }
                }
                SearchType::Content => {
                    // Match on diff line content
                    if let DisplayLine::Diff { line: diff_line, .. } = line {
                        pattern.is_match(&diff_line.content)
                    } else if let DisplayLine::FileHeader { path, .. } = line {
                        // Also match file headers in content search
                        pattern.is_match(path)
                    } else {
                        false
                    }
                }
            };

            if matches {
                search.matches.push(idx);
            }
        }

        // Jump to first match
        search.current_match = 0;
        if let Some(&idx) = search.matches.first() {
            self.current_line_idx = idx;
            self.ensure_cursor_on_navigable();
            self.adjust_scroll();
        }
    }

    fn next_search_match(&mut self) {
        let (new_idx, current, total) = {
            let Some(ref mut search) = self.search else {
                return;
            };

            if search.matches.is_empty() {
                return;
            }

            search.current_match = (search.current_match + 1) % search.matches.len();
            (search.matches[search.current_match], search.current_match + 1, search.matches.len())
        };

        self.current_line_idx = new_idx;
        self.adjust_scroll();
        self.message = Some(format!("Match {}/{}", current, total));
    }

    fn prev_search_match(&mut self) {
        let (new_idx, current, total) = {
            let Some(ref mut search) = self.search else {
                return;
            };

            if search.matches.is_empty() {
                return;
            }

            search.current_match = if search.current_match == 0 {
                search.matches.len() - 1
            } else {
                search.current_match - 1
            };
            (search.matches[search.current_match], search.current_match + 1, search.matches.len())
        };

        self.current_line_idx = new_idx;
        self.adjust_scroll();
        self.message = Some(format!("Match {}/{}", current, total));
    }

    /// Check if a display line is a change (addition or deletion)
    fn is_change_line(&self, idx: usize) -> bool {
        matches!(
            self.display_lines.get(idx),
            Some(DisplayLine::Diff { line, .. }) if line.kind == LineKind::Addition || line.kind == LineKind::Deletion
        )
    }

    /// Jump to next change chunk (group of consecutive additions/deletions)
    fn next_change_chunk(&mut self) {
        let total = self.display_lines.len();
        let mut idx = self.current_line_idx;

        // First, skip past current chunk if we're in one
        while idx < total && self.is_change_line(idx) {
            idx += 1;
        }

        // Now find the start of the next chunk
        while idx < total && !self.is_change_line(idx) {
            idx += 1;
        }

        if idx < total {
            self.current_line_idx = idx;
            self.adjust_scroll();
            return;
        }

        // Wrap around to beginning
        idx = 0;
        while idx < self.current_line_idx && !self.is_change_line(idx) {
            idx += 1;
        }
        if idx < self.current_line_idx && self.is_change_line(idx) {
            self.current_line_idx = idx;
            self.adjust_scroll();
        }
    }

    /// Jump to previous change chunk (group of consecutive additions/deletions)
    fn prev_change_chunk(&mut self) {
        let mut idx = self.current_line_idx;

        // If we're at the start of a chunk, move back one to get out of it
        if idx > 0 && self.is_change_line(idx) && !self.is_change_line(idx - 1) {
            idx -= 1;
        }

        // Skip back past current chunk if we're in one
        while idx > 0 && self.is_change_line(idx) {
            idx -= 1;
        }

        // Now go back to find a change line
        while idx > 0 && !self.is_change_line(idx) {
            idx -= 1;
        }

        // Now go to the start of that chunk
        while idx > 0 && self.is_change_line(idx - 1) {
            idx -= 1;
        }

        if self.is_change_line(idx) {
            self.current_line_idx = idx;
            self.adjust_scroll();
            return;
        }

        // Wrap around to end - find last chunk
        let total = self.display_lines.len();
        idx = total.saturating_sub(1);
        while idx > self.current_line_idx && !self.is_change_line(idx) {
            idx -= 1;
        }
        // Go to start of that chunk
        while idx > 0 && self.is_change_line(idx - 1) {
            idx -= 1;
        }
        if self.is_change_line(idx) {
            self.current_line_idx = idx;
            self.adjust_scroll();
        }
    }

    fn toggle_collapse_current_file(&mut self) {
        let current_header_idx = self.find_current_file_header_idx();
        let Some((_, file_idx)) = self.current_file_info() else {
            return;
        };

        let was_collapsed = self.collapsed_files.contains(&file_idx);
        if was_collapsed {
            self.collapsed_files.remove(&file_idx);
            self.message = Some("Expanded file".to_string());
        } else {
            self.collapsed_files.insert(file_idx);
            self.message = Some("Collapsed file".to_string());
        }

        self.build_display_lines();

        let header_idx = current_header_idx.and_then(|idx| {
            if let Some(DisplayLine::FileHeader { file_idx: header_file_idx, .. }) = self.display_lines.get(idx) {
                if *header_file_idx == file_idx {
                    return Some(idx);
                }
            }
            None
        }).or_else(|| {
            self.display_lines.iter().enumerate().find_map(|(idx, line)| {
                if let DisplayLine::FileHeader { file_idx: header_file_idx, .. } = line {
                    if *header_file_idx == file_idx {
                        return Some(idx);
                    }
                }
                None
            })
        });

        if let Some(idx) = header_idx {
            if was_collapsed {
                // Just expanded; move to first navigable line under the header if possible.
                let mut cursor = idx.saturating_add(1);
                while cursor < self.display_lines.len() && self.is_skippable_line(cursor) {
                    cursor += 1;
                }
                self.current_line_idx = cursor.min(self.display_lines.len().saturating_sub(1));
            } else {
                // Just collapsed; focus the header.
                self.current_line_idx = idx;
            }
            self.ensure_cursor_on_navigable();
            self.adjust_scroll();
        }
    }

    fn ensure_cursor_on_navigable(&mut self) {
        if self.display_lines.is_empty() {
            return;
        }
        if !self.is_skippable_line(self.current_line_idx) {
            return;
        }

        let mut down = self.current_line_idx;
        while down < self.display_lines.len() && self.is_skippable_line(down) {
            down += 1;
        }
        if down < self.display_lines.len() {
            self.current_line_idx = down;
            return;
        }

        let mut up = self.current_line_idx;
        while up > 0 && self.is_skippable_line(up) {
            up -= 1;
        }
        self.current_line_idx = up;
    }

    fn annotation_list_entries(&self) -> Vec<AnnotationListEntry> {
        let mut visible: HashMap<i64, usize> = HashMap::new();
        for (idx, line) in self.display_lines.iter().enumerate() {
            if let DisplayLine::Annotation { annotation, .. } = line {
                visible.insert(annotation.id, idx);
            }
        }

        let mut entries: Vec<AnnotationListEntry> = self
            .all_annotations
            .iter()
            .map(|a| AnnotationListEntry {
                id: a.id,
                file_path: a.file_path.clone(),
                line: a.start_line,
                side: a.side.clone(),
                annotation_type: a.annotation_type.clone(),
                content: a.content.clone(),
                display_idx: visible.get(&a.id).copied(),
            })
            .collect();

        entries.sort_by(|a, b| a.file_path.cmp(&b.file_path).then(a.line.cmp(&b.line)));
        entries
    }

    /// Adjust scroll to keep cursor visible with vim-like margins
    fn adjust_scroll(&mut self) {
        let scroll_margin = 5; // Keep 5 lines of padding

        // If cursor is above the visible area (with margin)
        if self.current_line_idx < self.scroll_offset + scroll_margin {
            self.scroll_offset = self.current_line_idx.saturating_sub(scroll_margin);
        }

        // If cursor is below the visible area (with margin)
        let bottom_threshold = self.scroll_offset + self.visible_height.saturating_sub(scroll_margin);
        if self.current_line_idx > bottom_threshold {
            self.scroll_offset = self.current_line_idx.saturating_sub(self.visible_height.saturating_sub(scroll_margin));
        }
    }
}

/// Runs the TUI application
pub fn run(
    storage: Storage,
    diff_engine: DiffEngine,
    repo_path: PathBuf,
    repo_id: i64,
    files: Vec<DiffFile>,
    config: Config,
) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(storage, diff_engine, repo_path, repo_id, files, config);
    app.load_all_annotations()?;
    app.build_display_lines();

    // Skip to first navigable line
    while app.current_line_idx < app.display_lines.len().saturating_sub(1)
        && app.is_skippable_line(app.current_line_idx)
    {
        app.current_line_idx += 1;
    }

    let result = run_app(&mut terminal, &mut app);

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    result
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        if let Event::Key(key) = event::read()? {
            if app.handle_input(key)? {
                return Ok(());
            }
        }
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let theme = Theme::default();

    // Expand input area when in annotation mode
    let input_height = match app.mode {
        Mode::Normal => 1,
        _ => {
            // Count newlines + 1 for current line, + 2 for border, max 10 lines
            let line_count = app.annotation_input.chars().filter(|&c| c == '\n').count() + 1;
            (line_count + 2).min(10) as u16
        }
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),            // Sticky file header
            Constraint::Min(0),               // Diff content
            Constraint::Length(input_height), // Status/input
        ])
        .split(f.area());

    // Update visible height for page navigation
    app.visible_height = chunks[1].height as usize;

    // Sticky file header
    render_sticky_file_header(f, app, chunks[0], theme);

    // Diff content
    if app.side_by_side {
        render_diff_side_by_side(f, app, chunks[1], theme);
    } else {
        render_diff_unified(f, app, chunks[1], theme);
    }

    // Status bar / input
    render_status(f, app, chunks[2], theme);

    // Help overlay
    if app.show_help {
        render_help(f, theme);
    }

    if matches!(app.mode, Mode::AnnotationList) {
        render_annotation_list(f, app, theme);
    }
}

fn render_sticky_file_header(f: &mut Frame, app: &App, area: Rect, theme: Theme) {
    // Find the current file's header index
    let current_file_header_idx = app.find_current_file_header_idx();

    // Check if the file header is visible on screen (use app.visible_height, not area.height)
    let visible_start = app.scroll_offset;
    let visible_end = app.scroll_offset + app.visible_height;
    let header_is_visible = current_file_header_idx
        .map(|idx| idx >= visible_start && idx < visible_end)
        .unwrap_or(false);

    // If the header is visible on screen, don't show the sticky header
    if header_is_visible {
        // Render an empty line or minimal separator
        let paragraph = Paragraph::new("")
            .style(Style::default().bg(theme.status_bg));
        f.render_widget(paragraph, area);
        return;
    }

    let (file_path, file_idx) = app.current_file_info().unwrap_or(("<none>".to_string(), 0));
    let is_collapsed = app.collapsed_files.contains(&file_idx);

    // Calculate file stats
    let file = app.files.get(file_idx);
    let (additions, deletions) = file.map(|f| {
        let adds: usize = f.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == LineKind::Addition).count();
        let dels: usize = f.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == LineKind::Deletion).count();
        (adds, dels)
    }).unwrap_or((0, 0));

    // Check if this file is a search match and if it's the current match
    let is_current_match = current_file_header_idx
        .map(|idx| app.is_current_search_match(idx))
        .unwrap_or(false);
    let is_search_match = app.is_file_search_match(file_idx);

    // Different style when expanded or search match
    let (style, expanded_indicator) = if app.expanded_file.is_some() {
        (
            Style::default()
                .fg(theme.expanded_fg)
                .bg(theme.expanded_bg)
                .add_modifier(Modifier::BOLD),
            " [FULL FILE - x to collapse]"
        )
    } else if is_current_match {
        (
            Style::default()
                .fg(theme.header_match_fg)
                .bg(theme.header_match_bg)
                .add_modifier(Modifier::BOLD),
            ""
        )
    } else if is_search_match {
        (
            Style::default()
                .fg(theme.header_dim_fg)
                .bg(theme.header_dim_bg)
                .add_modifier(Modifier::BOLD),
            ""
        )
    } else {
        (
            Style::default()
                .fg(theme.header_fg)
                .bg(theme.header_bg)
                .add_modifier(Modifier::BOLD),
            ""
        )
    };

    let collapse_indicator = if is_collapsed { " ▶" } else { "" };

    let header = format!(
        " Δ {}  [+{} -{}]  ({}/{}){}{}",
        file_path,
        additions,
        deletions,
        file_idx + 1,
        app.files.len(),
        expanded_indicator,
        collapse_indicator
    );

    let paragraph = Paragraph::new(header).style(style);
    f.render_widget(paragraph, area);
}


fn render_diff_unified(f: &mut Frame, app: &App, area: Rect, theme: Theme) {
    let visible_height = area.height as usize;
    let scroll_offset = app.scroll_offset;

    let items: Vec<ListItem> = app
        .display_lines
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_height)
        .map(|(idx, display_line)| {
            let is_current = idx == app.current_line_idx;

            match display_line {
                DisplayLine::Spacer => {
                    ListItem::new(Line::from(""))
                }
                DisplayLine::FileHeader { path, file_idx } => {
                    // Show file header prominently in content
                    let file = app.files.get(*file_idx);
                    let (adds, dels) = file.map(|f| {
                        let a: usize = f.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == LineKind::Addition).count();
                        let d: usize = f.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == LineKind::Deletion).count();
                        (a, d)
                    }).unwrap_or((0, 0));

                    // Check if this is the current search match or just a match
                    let is_current_match = app.is_current_search_match(idx);
                    let is_match = app.is_search_match(idx);

                    let (mut style, stats_bg) = if is_current_match {
                        // Current match: bright yellow background
                        (
                            Style::default()
                                .fg(theme.header_match_fg)
                                .bg(theme.header_match_bg)
                                .add_modifier(Modifier::BOLD),
                            theme.header_match_bg
                        )
                    } else if is_match {
                        // Other matches: dimmer highlight
                        (
                            Style::default()
                                .fg(theme.header_dim_fg)
                                .bg(theme.header_dim_bg)
                                .add_modifier(Modifier::BOLD),
                            theme.header_dim_bg
                        )
                    } else {
                        (
                            Style::default()
                                .fg(theme.header_fg)
                                .bg(theme.header_bg)
                                .add_modifier(Modifier::BOLD),
                            theme.header_bg
                        )
                    };

                    if is_current {
                        style = style
                            .bg(theme.header_focus_bg)
                            .add_modifier(Modifier::UNDERLINED);
                    }

                    let collapse_indicator = if app.collapsed_files.contains(file_idx) { " ▶" } else { "" };

                    ListItem::new(Line::from(vec![
                        Span::styled(format!(" Δ {}{}  ", path, collapse_indicator), style),
                        Span::styled(format!("+{} ", adds), Style::default().fg(theme.added_fg).bg(stats_bg)),
                        Span::styled(format!("-{} ", dels), Style::default().fg(theme.deleted_fg).bg(stats_bg)),
                    ]))
                }
                DisplayLine::HunkHeader { line_no, additions, deletions, .. } => {
                    // Hunk header - prominent with stats
                    let style = Style::default()
                        .fg(theme.hunk_fg)
                        .bg(theme.hunk_bg);
                    let stats_style = Style::default()
                        .fg(theme.hunk_fg)
                        .bg(theme.hunk_bg)
                        .add_modifier(Modifier::BOLD);
                    ListItem::new(Line::from(vec![
                        Span::styled(format!(" +{} -{} ", additions, deletions), stats_style),
                        Span::styled(format!("starting at line {} ", line_no), style),
                    ]))
                }
                DisplayLine::HunkEnd => {
                    // End of hunk - subtle bottom border
                    let style = Style::default().fg(theme.hunk_border);
                    ListItem::new(Line::from(Span::styled("╰".to_string() + &"─".repeat(30), style)))
                }
                DisplayLine::Diff { line, file_path, .. } => {
                    let line_no = line.new_line_no.or(line.old_line_no).unwrap_or(0);
                    let side = if line.new_line_no.is_some() {
                        Side::New
                    } else {
                        Side::Old
                    };

                    // Check for annotation
                    let has_annotation = app.all_annotations.iter().any(|a| {
                        a.file_path == *file_path
                            && a.side == side
                            && a.start_line <= line_no
                            && a.end_line.map_or(a.start_line == line_no, |e| e >= line_no)
                    });
                    let annotation_marker = if has_annotation { "●" } else { " " };

                    let line_num_style = if is_current {
                        Style::default().fg(theme.line_num).bg(theme.current_line_bg)
                    } else {
                        Style::default().fg(theme.line_num)
                    };
                    let line_num = format!("{:>4}", line_no);

                    let (prefix, content_style) = match line.kind {
                        LineKind::Addition => (
                            "+",
                            Style::default().fg(theme.added_fg).bg(theme.added_bg),
                        ),
                        LineKind::Deletion => (
                            "-",
                            Style::default().fg(theme.deleted_fg).bg(theme.deleted_bg),
                        ),
                        LineKind::Context => (
                            " ",
                            Style::default().fg(theme.context_fg),
                        ),
                    };

                    // Build spans with syntax highlighting
                    let marker_style = if is_current {
                        Style::default()
                            .fg(theme.annotation_marker)
                            .bg(theme.current_line_bg)
                    } else {
                        Style::default().fg(theme.annotation_marker)
                    };

                    let mut spans = vec![
                        Span::styled(annotation_marker, marker_style),
                        Span::styled(line_num, line_num_style),
                        Span::raw(" "),
                    ];

                    let content_spans = build_highlighted_spans(
                        prefix,
                        &line.content,
                        &line.highlights,
                        content_style,
                        is_current,
                        theme,
                    );
                    spans.extend(content_spans);

                    ListItem::new(Line::from(spans))
                }
                DisplayLine::Annotation { annotation, .. } => {
                    // Annotation in a prominent box - handle multiple lines
                    let (prefix, style) = match annotation.annotation_type {
                        AnnotationType::Comment => (
                            "    💬 ",
                            Style::default().fg(theme.annotation_fg).bg(theme.annotation_bg),
                        ),
                        AnnotationType::Todo => (
                            "    📌 ",
                            Style::default()
                                .fg(theme.todo_fg)
                                .bg(theme.todo_bg)
                                .add_modifier(Modifier::BOLD),
                        ),
                    };

                    // Split content by newlines and create multiple lines
                    let lines: Vec<Line> = annotation.content
                        .lines()
                        .enumerate()
                        .map(|(i, line)| {
                            let prefix = if i == 0 {
                                prefix.to_string()
                            } else {
                                "       ".to_string() // Indent continuation lines
                            };
                            Line::from(Span::styled(format!("{}{} ", prefix, line), style))
                        })
                        .collect();

                    ListItem::new(lines)
                }
            }
        })
        .collect();

    // No borders for cleaner look
    let diff_list = List::new(items);

    f.render_widget(diff_list, area);
}

fn render_diff_side_by_side(f: &mut Frame, app: &App, area: Rect, theme: Theme) {
    // Split the area into two columns with a small gap
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(49),
            Constraint::Length(2), // Divider
            Constraint::Percentage(49),
        ])
        .split(area);

    let visible_height = area.height as usize;
    let scroll_offset = app.scroll_offset;

    // Build left (old) and right (new) items
    let mut left_items: Vec<ListItem> = Vec::new();
    let mut right_items: Vec<ListItem> = Vec::new();

    for (idx, display_line) in app.display_lines.iter().enumerate().skip(scroll_offset).take(visible_height) {
        let is_current = idx == app.current_line_idx;

        match display_line {
            DisplayLine::Spacer => {
                left_items.push(ListItem::new(Line::from("")));
                right_items.push(ListItem::new(Line::from("")));
            }
            DisplayLine::FileHeader { path, file_idx } => {
                // Show file header prominently
                let file = app.files.get(*file_idx);
                let (adds, dels) = file.map(|f| {
                    let a: usize = f.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == LineKind::Addition).count();
                    let d: usize = f.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == LineKind::Deletion).count();
                    (a, d)
                }).unwrap_or((0, 0));

                // Check if this is the current search match or just a match
                let is_current_match = app.is_current_search_match(idx);
                let is_match = app.is_search_match(idx);

                let (mut style, stats_bg) = if is_current_match {
                    // Current match: bright yellow background
                    (
                        Style::default()
                            .fg(theme.header_match_fg)
                            .bg(theme.header_match_bg)
                            .add_modifier(Modifier::BOLD),
                        theme.header_match_bg
                    )
                } else if is_match {
                    // Other matches: dimmer highlight
                    (
                        Style::default()
                            .fg(theme.header_dim_fg)
                            .bg(theme.header_dim_bg)
                            .add_modifier(Modifier::BOLD),
                        theme.header_dim_bg
                    )
                } else {
                    (
                        Style::default()
                            .fg(theme.header_fg)
                            .bg(theme.header_bg)
                            .add_modifier(Modifier::BOLD),
                        theme.header_bg
                    )
                };

                if is_current {
                    style = style
                        .bg(theme.header_focus_bg)
                        .add_modifier(Modifier::UNDERLINED);
                }

                let collapse_indicator = if app.collapsed_files.contains(file_idx) { " ▶" } else { "" };

                let header = Line::from(vec![
                    Span::styled(format!(" Δ {}{} ", path, collapse_indicator), style),
                    Span::styled(format!("+{} ", adds), Style::default().fg(theme.added_fg).bg(stats_bg)),
                    Span::styled(format!("-{} ", dels), Style::default().fg(theme.deleted_fg).bg(stats_bg)),
                ]);
                left_items.push(ListItem::new(header));
                right_items.push(ListItem::new(Line::from("")));
            }
            DisplayLine::HunkHeader { line_no, additions, deletions, .. } => {
                // Hunk header - prominent with stats
                let style = Style::default()
                    .fg(theme.hunk_fg)
                    .bg(theme.hunk_bg);
                let stats_style = Style::default()
                    .fg(theme.hunk_fg)
                    .bg(theme.hunk_bg)
                    .add_modifier(Modifier::BOLD);
                left_items.push(ListItem::new(Line::from(vec![
                    Span::styled(format!(" +{} -{} ", additions, deletions), stats_style),
                    Span::styled(format!("starting at line {} ", line_no), style),
                ])));
                right_items.push(ListItem::new(Line::from("")));
            }
            DisplayLine::HunkEnd => {
                // End of hunk - subtle bottom border
                let style = Style::default().fg(theme.hunk_border);
                left_items.push(ListItem::new(Line::from(Span::styled("╰".to_string() + &"─".repeat(20), style))));
                right_items.push(ListItem::new(Line::from(Span::styled("╰".to_string() + &"─".repeat(20), style))));
            }
            DisplayLine::Diff { line, file_path, .. } => {
                let line_no = line.new_line_no.or(line.old_line_no).unwrap_or(0);
                let side = if line.new_line_no.is_some() {
                    Side::New
                } else {
                    Side::Old
                };

                // Check for annotation
                let has_annotation = app.all_annotations.iter().any(|a| {
                    a.file_path == *file_path
                        && a.side == side
                        && a.start_line <= line_no
                        && a.end_line.map_or(a.start_line == line_no, |e| e >= line_no)
                });
                let line_num_style = if is_current {
                    Style::default().fg(theme.line_num).bg(theme.current_line_bg)
                } else {
                    Style::default().fg(theme.line_num)
                };
                let marker_style = if is_current {
                    Style::default()
                        .fg(theme.annotation_marker)
                        .bg(theme.current_line_bg)
                } else {
                    Style::default().fg(theme.annotation_marker)
                };
                let marker = if has_annotation { "●" } else { " " };

                match line.kind {
                    LineKind::Deletion => {
                        let old_no = line.old_line_no.map_or("    ".to_string(), |n| format!("{:>4}", n));
                        let content_style = Style::default().fg(theme.deleted_fg).bg(theme.deleted_bg);
                        let mut left_spans = vec![
                            Span::styled(marker, marker_style),
                            Span::styled(old_no, line_num_style),
                        ];
                        left_spans.extend(build_highlighted_spans("-", &line.content, &line.highlights, content_style, is_current, theme));
                        left_items.push(ListItem::new(Line::from(left_spans)));
                        right_items.push(ListItem::new(Line::from("")));
                    }
                    LineKind::Addition => {
                        let new_no = line.new_line_no.map_or("    ".to_string(), |n| format!("{:>4}", n));
                        let content_style = Style::default().fg(theme.added_fg).bg(theme.added_bg);
                        left_items.push(ListItem::new(Line::from("")));
                        let mut right_spans = vec![
                            Span::styled(marker, marker_style),
                            Span::styled(new_no, line_num_style),
                        ];
                        right_spans.extend(build_highlighted_spans("+", &line.content, &line.highlights, content_style, is_current, theme));
                        right_items.push(ListItem::new(Line::from(right_spans)));
                    }
                    LineKind::Context => {
                        let old_no = line.old_line_no.map_or("    ".to_string(), |n| format!("{:>4}", n));
                        let new_no = line.new_line_no.map_or("    ".to_string(), |n| format!("{:>4}", n));
                        let content_style = Style::default().fg(theme.context_fg);
                        let mut left_spans = vec![
                            Span::styled(marker, marker_style),
                            Span::styled(old_no, line_num_style),
                        ];
                        left_spans.extend(build_highlighted_spans(" ", &line.content, &line.highlights, content_style, is_current, theme));
                        left_items.push(ListItem::new(Line::from(left_spans)));

                        let mut right_spans = vec![
                            Span::styled(marker, marker_style),
                            Span::styled(new_no, line_num_style),
                        ];
                        right_spans.extend(build_highlighted_spans(" ", &line.content, &line.highlights, content_style, is_current, theme));
                        right_items.push(ListItem::new(Line::from(right_spans)));
                    }
                }
            }
            DisplayLine::Annotation { annotation, .. } => {
                // Annotation in a prominent box - handle multiple lines
                let (prefix, style) = match annotation.annotation_type {
                    AnnotationType::Comment => (
                        "   💬 ",
                        Style::default().fg(theme.annotation_fg).bg(theme.annotation_bg),
                    ),
                    AnnotationType::Todo => (
                        "   📌 ",
                        Style::default()
                            .fg(theme.todo_fg)
                            .bg(theme.todo_bg)
                            .add_modifier(Modifier::BOLD),
                    ),
                };

                // Split content by newlines and create multiple lines
                let lines: Vec<Line> = annotation.content
                    .lines()
                    .enumerate()
                    .map(|(i, line)| {
                        let prefix = if i == 0 {
                            prefix.to_string()
                        } else {
                            "      ".to_string()
                        };
                        Line::from(Span::styled(format!("{}{} ", prefix, line), style))
                    })
                    .collect();

                // Show annotation on the appropriate side
                match annotation.side {
                    Side::Old => {
                        left_items.push(ListItem::new(lines));
                        right_items.push(ListItem::new(Line::from("")));
                    }
                    Side::New => {
                        left_items.push(ListItem::new(Line::from("")));
                        right_items.push(ListItem::new(lines));
                    }
                }
            }
        }
    }

    // No borders, cleaner look
    let left_list = List::new(left_items);
    let right_list = List::new(right_items);

    f.render_widget(left_list, columns[0]);
    f.render_widget(right_list, columns[2]);
}

fn render_status(f: &mut Frame, app: &App, area: Rect, theme: Theme) {
    match &app.mode {
        Mode::Normal => {
            let content = if let Some(msg) = &app.message {
                msg.clone()
            } else if app.expanded_file.is_some() {
                " j/k:line  n/N:chunk  ^d/^u:page  x:collapse  c:collapse  A:list  a:annotate  ?:help  q:quit".to_string()
            } else if app.search.is_some() {
                " n/N:match  Esc:clear  j/k:nav  Tab:file  c:collapse  x:expand  A:list  a:annotate  ?:help  q:quit".to_string()
            } else {
                " j/k:nav  n/N:chunk  Tab:file  c:collapse  f:find  /:search  x:expand  A:list  a:annotate  ?:help  q:quit".to_string()
            };
            let status = Paragraph::new(content)
                .style(Style::default().fg(theme.status_fg).bg(theme.status_bg));
            f.render_widget(status, area);
        }
        Mode::AddAnnotation | Mode::EditAnnotation(_) => {
            // Multi-line input area with border
            let title = match &app.mode {
                Mode::AddAnnotation => format!(
                    " Add {} (^J: newline, Ctrl+T: type, Enter: save, Esc: cancel) ",
                    app.annotation_type.as_str()
                ),
                Mode::EditAnnotation(_) => format!(
                    " Edit {} (^J: newline, Ctrl+T: type, Enter: save, Esc: cancel) ",
                    app.annotation_type.as_str()
                ),
                _ => String::new(),
            };

            let input_with_cursor = format!("{}_", app.annotation_input);

            let input = Paragraph::new(input_with_cursor)
                .style(Style::default().fg(theme.header_fg).bg(theme.surface_alt))
                .block(Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border))
                    .title(title));

            f.render_widget(input, area);
        }
        Mode::SearchFile | Mode::SearchContent => {
            let search_type = match &app.mode {
                Mode::SearchFile => "file",
                Mode::SearchContent => "content",
                _ => "",
            };

            let match_info = if let Some(ref search) = app.search {
                if search.matches.is_empty() {
                    if search.query.is_empty() {
                        String::new()
                    } else {
                        " (no matches)".to_string()
                    }
                } else {
                    format!(" ({} match{})", search.matches.len(), if search.matches.len() == 1 { "" } else { "es" })
                }
            } else {
                String::new()
            };

            let query = app.search.as_ref().map(|s| s.query.as_str()).unwrap_or("");
            let content = format!(" Search {}: {}_{}  (Enter: confirm, Esc: cancel)", search_type, query, match_info);

            let status = Paragraph::new(content)
                .style(Style::default().fg(theme.search_fg).bg(theme.search_bg));
            f.render_widget(status, area);
        }
        Mode::AnnotationList => {
            let status = Paragraph::new(" Annotations: j/k, Enter: jump, e: edit, d: delete, Esc: close")
                .style(Style::default().fg(theme.status_fg).bg(theme.status_bg));
            f.render_widget(status, area);
        }
    }
}

fn render_help(f: &mut Frame, theme: Theme) {
    let area = centered_rect(60, 80, f.area());

    let help_text = vec![
        "",
        "  Navigation:",
        "    j / Down  Move down line by line",
        "    k / Up    Move up line by line",
        "    n / N     Next / previous chunk",
        "    Tab       Next file",
        "    Shift+Tab Previous file",
        "    Ctrl+d    Half page down",
        "    Ctrl+u    Half page up",
        "    g         Go to start",
        "    G         Go to end",
        "",
        "  Search:",
        "    f         Search by filename (regex)",
        "    /         Search in content (regex)",
        "    n / N     Next / previous match (when searching)",
        "    Esc       Clear search",
        "",
        "  View:",
        "    x         Expand/collapse current file (full view)",
        "    c         Collapse/expand current file",
        "    s         Toggle side-by-side view",
        "    A         Annotation list",
        "",
        "  Annotations:",
        "    a         Add annotation at current line",
        "    e         Edit annotation at current line",
        "    d         Delete annotation at current line",
        "    t         Toggle annotation type (comment/todo)",
        "",
        "  Other:",
        "    ?         Toggle this help",
        "    q         Quit",
        "",
        "  In annotation mode:",
        "    Enter     Save annotation",
        "    Ctrl+j    Add newline",
        "    Ctrl+t    Toggle annotation type",
        "    Esc       Cancel",
        "",
    ];

    let help = Paragraph::new(help_text.join("\n"))
        .style(Style::default().fg(theme.help_fg).bg(theme.help_bg))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Help ")
                .style(Style::default().bg(theme.help_bg))
                .border_style(Style::default().fg(theme.border)),
        )
        .wrap(Wrap { trim: false });

    f.render_widget(Clear, area);
    f.render_widget(help, area);
}

#[derive(Clone)]
struct AnnotationListEntry {
    id: i64,
    file_path: String,
    line: u32,
    side: Side,
    annotation_type: AnnotationType,
    content: String,
    display_idx: Option<usize>,
}

fn render_annotation_list(f: &mut Frame, app: &mut App, theme: Theme) {
    let entries = app.annotation_list_entries();
    let area = centered_rect(80, 80, f.area());

    if entries.is_empty() {
        let empty = Paragraph::new("No annotations")
            .style(Style::default().fg(theme.help_fg).bg(theme.help_bg))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Annotations ")
                    .border_style(Style::default().fg(theme.border)),
            );
        f.render_widget(Clear, area);
        f.render_widget(empty, area);
        return;
    }

    let visible_height = area.height.saturating_sub(2) as usize;
    let start = app
        .annotation_list_idx
        .saturating_sub(visible_height / 2);
    let end = (start + visible_height).min(entries.len());

    let lines: Vec<Line> = entries[start..end]
        .iter()
        .enumerate()
        .map(|(offset, entry)| {
            let idx = start + offset;
            let is_selected = idx == app.annotation_list_idx;
            let is_orphaned = entry.display_idx.is_none();

            let type_marker = match entry.annotation_type {
                AnnotationType::Comment => "💬",
                AnnotationType::Todo => "📌",
            };
            let side_marker = match entry.side {
                Side::Old => "old",
                Side::New => "new",
            };

            let mut content = entry.content.replace('\n', " ");
            if content.len() > 60 {
                content.truncate(57);
                content.push_str("...");
            }

            let mut label = format!(
                "{} {}:{} [{}] {}",
                type_marker, entry.file_path, entry.line, side_marker, content
            );
            if is_orphaned {
                label.push_str("  (orphaned)");
            }

            let mut style = if is_orphaned {
                Style::default().fg(theme.deleted_fg)
            } else {
                Style::default().fg(theme.help_fg)
            };

            if is_selected {
                style = style.bg(theme.header_focus_bg).add_modifier(Modifier::BOLD);
            }

            Line::from(Span::styled(label, style))
        })
        .collect();

    let list = Paragraph::new(lines)
        .style(Style::default().bg(theme.help_bg))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Annotations (j/k, Enter, e, d, Esc) ")
                .border_style(Style::default().fg(theme.border)),
        );

    f.render_widget(Clear, area);
    f.render_widget(list, area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Get the color for a syntax token type
fn token_color(token_type: TokenType, theme: Theme) -> Color {
    match token_type {
        TokenType::Keyword => theme.token_keyword,
        TokenType::String => theme.token_string,
        TokenType::Comment => theme.token_comment,
        TokenType::Function => theme.token_function,
        TokenType::Type => theme.token_type,
        TokenType::Number => theme.token_number,
        TokenType::Operator => theme.token_operator,
        TokenType::Variable => theme.token_variable,
        TokenType::Atom => theme.token_atom,
        TokenType::Module => theme.token_module,
        TokenType::Default => theme.token_default,
    }
}

/// Build spans with syntax highlighting for a diff line
fn build_highlighted_spans<'a>(
    prefix: &'a str,
    content: &'a str,
    highlights: &[HighlightRange],
    base_style: Style,
    is_current: bool,
    theme: Theme,
) -> Vec<Span<'a>> {
    let base_style = if is_current {
        base_style
            .bg(theme.current_line_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        base_style
    };

    let mut spans = vec![Span::styled(format!("{} ", prefix), base_style)];

    if highlights.is_empty() {
        spans.push(Span::styled(content.to_string(), base_style));
        return spans;
    }

    let content_bytes = content.as_bytes();
    let mut pos = 0;

    for highlight in highlights {
        // Ensure we don't go out of bounds
        let start = highlight.start.min(content_bytes.len());
        let end = highlight.end.min(content_bytes.len());

        if start > pos {
            // Add unhighlighted segment
            if let Ok(segment) = std::str::from_utf8(&content_bytes[pos..start]) {
                spans.push(Span::styled(segment.to_string(), base_style));
            }
        }

        if end > start {
            // Add highlighted segment
            if let Ok(segment) = std::str::from_utf8(&content_bytes[start..end]) {
                let color = token_color(highlight.token_type, theme);
                let style = if is_current {
                    // Keep diff background, just increase emphasis
                    base_style.fg(color).add_modifier(Modifier::BOLD)
                } else {
                    // Preserve background from base_style
                    base_style.fg(color)
                };
                spans.push(Span::styled(segment.to_string(), style));
            }
        }

        pos = end;
    }

    // Add remaining unhighlighted content
    if pos < content_bytes.len() {
        if let Ok(segment) = std::str::from_utf8(&content_bytes[pos..]) {
            spans.push(Span::styled(segment.to_string(), base_style));
        }
    }

    spans
}
