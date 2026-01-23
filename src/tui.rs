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
use std::io::{self, Stdout};
use std::path::PathBuf;

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
    // Position to restore when collapsing expanded view
    pre_expand_position: Option<(usize, usize)>, // (line_idx, scroll_offset)

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
            pre_expand_position: None,
            search: None,
        }
    }

    /// Load all annotations for all files and build the display lines
    fn load_all_annotations(&mut self) -> Result<()> {
        self.all_annotations.clear();

        for file in &self.files {
            let path = file
                .new_path
                .as_ref()
                .or(file.old_path.as_ref())
                .map(|p| p.to_string_lossy().to_string());

            if let Some(path) = path {
                let annotations = self.storage.list_annotations(self.repo_id, Some(&path))?;
                self.all_annotations.extend(annotations);
            }
        }

        Ok(())
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

            self.storage.add_annotation(
                self.repo_id,
                &file_path,
                None, // commit_sha
                side,
                line_no,
                None, // end_line
                self.annotation_type.clone(),
                &self.annotation_input,
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
        self.storage.update_annotation(id, &self.annotation_input)?;
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
        matches!(
            self.display_lines.get(idx),
            Some(DisplayLine::Annotation { .. })
                | Some(DisplayLine::HunkHeader { .. })
                | Some(DisplayLine::HunkEnd)
                | Some(DisplayLine::Spacer)
                | Some(DisplayLine::FileHeader { .. })
        )
    }

    fn navigate_up(&mut self) {
        if self.current_line_idx > 0 {
            self.current_line_idx -= 1;
            // Skip over non-navigable lines
            while self.current_line_idx > 0 && self.is_skippable_line(self.current_line_idx) {
                self.current_line_idx -= 1;
            }
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
                self.search = None;
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
                self.show_annotations = !self.show_annotations;
                self.build_display_lines();
                // Clamp current line index
                if self.current_line_idx >= self.display_lines.len() {
                    self.current_line_idx = self.display_lines.len().saturating_sub(1);
                }
                self.message = Some(format!(
                    "Annotations {}",
                    if self.show_annotations { "shown" } else { "hidden" }
                ));
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
                if let Some(annotation) = self.get_annotation_for_current_line() {
                    let id = annotation.id;
                    let content = annotation.content.clone();
                    self.annotation_input = content;
                    self.mode = Mode::EditAnnotation(id);
                }
            }
            KeyCode::Char('d') => self.delete_annotation_at_line()?,

            // Annotation type toggle
            KeyCode::Char('t') => {
                self.annotation_type = match self.annotation_type {
                    AnnotationType::Comment => AnnotationType::Todo,
                    AnnotationType::Todo => AnnotationType::Comment,
                };
                self.message = Some(format!(
                    "Annotation type: {}",
                    self.annotation_type.as_str()
                ));
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
    render_sticky_file_header(f, app, chunks[0]);

    // Diff content
    if app.side_by_side {
        render_diff_side_by_side(f, app, chunks[1]);
    } else {
        render_diff_unified(f, app, chunks[1]);
    }

    // Status bar / input
    render_status(f, app, chunks[2]);

    // Help overlay
    if app.show_help {
        render_help(f);
    }
}

fn render_sticky_file_header(f: &mut Frame, app: &App, area: Rect) {
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
            .style(Style::default().bg(Color::Rgb(30, 30, 30)));
        f.render_widget(paragraph, area);
        return;
    }

    let (file_path, file_idx) = app.current_file_info().unwrap_or(("<none>".to_string(), 0));

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
                .fg(Color::White)
                .bg(Color::Rgb(80, 60, 40)) // Orange-ish when expanded
                .add_modifier(Modifier::BOLD),
            " [FULL FILE - x to collapse]"
        )
    } else if is_current_match {
        (
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow) // Bright yellow for current match
                .add_modifier(Modifier::BOLD),
            ""
        )
    } else if is_search_match {
        (
            Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(180, 180, 100)) // Dimmer for other matches
                .add_modifier(Modifier::BOLD),
            ""
        )
    } else {
        (
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(40, 40, 60))
                .add_modifier(Modifier::BOLD),
            ""
        )
    };

    let header = format!(
        " Δ {}  [+{} -{}]  ({}/{}){}",
        file_path,
        additions,
        deletions,
        file_idx + 1,
        app.files.len(),
        expanded_indicator
    );

    let paragraph = Paragraph::new(header).style(style);
    f.render_widget(paragraph, area);
}


fn render_diff_unified(f: &mut Frame, app: &App, area: Rect) {
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

                    let (style, stats_bg) = if is_current_match {
                        // Current match: bright yellow background
                        (
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                            Color::Yellow
                        )
                    } else if is_match {
                        // Other matches: dimmer highlight
                        (
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Rgb(180, 180, 100))
                                .add_modifier(Modifier::BOLD),
                            Color::Rgb(180, 180, 100)
                        )
                    } else {
                        (
                            Style::default()
                                .fg(Color::White)
                                .bg(Color::Rgb(40, 40, 60))
                                .add_modifier(Modifier::BOLD),
                            Color::Rgb(40, 40, 60)
                        )
                    };

                    ListItem::new(Line::from(vec![
                        Span::styled(format!(" Δ {}  ", path), style),
                        Span::styled(format!("+{} ", adds), Style::default().fg(Color::Green).bg(stats_bg)),
                        Span::styled(format!("-{} ", dels), Style::default().fg(Color::Red).bg(stats_bg)),
                    ]))
                }
                DisplayLine::HunkHeader { line_no, additions, deletions, .. } => {
                    // Hunk header - prominent with stats
                    let style = Style::default()
                        .fg(Color::Black)
                        .bg(Color::Rgb(180, 180, 100));
                    let stats_style = Style::default()
                        .fg(Color::Black)
                        .bg(Color::Rgb(180, 180, 100))
                        .add_modifier(Modifier::BOLD);
                    ListItem::new(Line::from(vec![
                        Span::styled(format!(" +{} -{} ", additions, deletions), stats_style),
                        Span::styled(format!("starting at line {} ", line_no), style),
                    ]))
                }
                DisplayLine::HunkEnd => {
                    // End of hunk - subtle bottom border
                    let style = Style::default().fg(Color::Rgb(60, 60, 40));
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

                    let line_num_style = Style::default().fg(Color::DarkGray);
                    let line_num = format!("{:>4}", line_no);

                    let (prefix, content_style) = match line.kind {
                        LineKind::Addition => (
                            "+",
                            Style::default().fg(Color::Green).bg(Color::Rgb(0, 40, 0)),
                        ),
                        LineKind::Deletion => (
                            "-",
                            Style::default().fg(Color::Red).bg(Color::Rgb(40, 0, 0)),
                        ),
                        LineKind::Context => (
                            " ",
                            Style::default().fg(Color::White),
                        ),
                    };

                    // Build spans with syntax highlighting
                    let mut spans = vec![
                        Span::styled(annotation_marker, Style::default().fg(Color::Yellow)),
                        Span::styled(line_num, line_num_style),
                        Span::raw(" "),
                    ];

                    let content_spans = build_highlighted_spans(
                        prefix,
                        &line.content,
                        &line.highlights,
                        content_style,
                        is_current,
                    );
                    spans.extend(content_spans);

                    ListItem::new(Line::from(spans))
                }
                DisplayLine::Annotation { annotation, .. } => {
                    // Annotation in a prominent box - handle multiple lines
                    let icon = match annotation.annotation_type {
                        AnnotationType::Comment => "💬",
                        AnnotationType::Todo => "📋",
                    };
                    let style = Style::default()
                        .fg(Color::White)
                        .bg(Color::Rgb(50, 80, 80));

                    // Split content by newlines and create multiple lines
                    let lines: Vec<Line> = annotation.content
                        .lines()
                        .enumerate()
                        .map(|(i, line)| {
                            let prefix = if i == 0 {
                                format!("    {} ", icon)
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

fn render_diff_side_by_side(f: &mut Frame, app: &App, area: Rect) {
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

    let line_num_style = Style::default().fg(Color::DarkGray);

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

                let (style, stats_bg) = if is_current_match {
                    // Current match: bright yellow background
                    (
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                        Color::Yellow
                    )
                } else if is_match {
                    // Other matches: dimmer highlight
                    (
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Rgb(180, 180, 100))
                            .add_modifier(Modifier::BOLD),
                        Color::Rgb(180, 180, 100)
                    )
                } else {
                    (
                        Style::default()
                            .fg(Color::White)
                            .bg(Color::Rgb(40, 40, 60))
                            .add_modifier(Modifier::BOLD),
                        Color::Rgb(40, 40, 60)
                    )
                };

                let header = Line::from(vec![
                    Span::styled(format!(" Δ {} ", path), style),
                    Span::styled(format!("+{} ", adds), Style::default().fg(Color::Green).bg(stats_bg)),
                    Span::styled(format!("-{} ", dels), Style::default().fg(Color::Red).bg(stats_bg)),
                ]);
                left_items.push(ListItem::new(header));
                right_items.push(ListItem::new(Line::from("")));
            }
            DisplayLine::HunkHeader { line_no, additions, deletions, .. } => {
                // Hunk header - prominent with stats
                let style = Style::default()
                    .fg(Color::Black)
                    .bg(Color::Rgb(180, 180, 100));
                let stats_style = Style::default()
                    .fg(Color::Black)
                    .bg(Color::Rgb(180, 180, 100))
                    .add_modifier(Modifier::BOLD);
                left_items.push(ListItem::new(Line::from(vec![
                    Span::styled(format!(" +{} -{} ", additions, deletions), stats_style),
                    Span::styled(format!("starting at line {} ", line_no), style),
                ])));
                right_items.push(ListItem::new(Line::from("")));
            }
            DisplayLine::HunkEnd => {
                // End of hunk - subtle bottom border
                let style = Style::default().fg(Color::Rgb(60, 60, 40));
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
                let marker_style = Style::default().fg(Color::Yellow);
                let marker = if has_annotation { "●" } else { " " };

                match line.kind {
                    LineKind::Deletion => {
                        let old_no = line.old_line_no.map_or("    ".to_string(), |n| format!("{:>4}", n));
                        let content_style = Style::default().fg(Color::Red).bg(Color::Rgb(40, 0, 0));
                        let mut left_spans = vec![
                            Span::styled(marker, marker_style),
                            Span::styled(old_no, line_num_style),
                        ];
                        left_spans.extend(build_highlighted_spans("-", &line.content, &line.highlights, content_style, is_current));
                        left_items.push(ListItem::new(Line::from(left_spans)));
                        right_items.push(ListItem::new(Line::from("")));
                    }
                    LineKind::Addition => {
                        let new_no = line.new_line_no.map_or("    ".to_string(), |n| format!("{:>4}", n));
                        let content_style = Style::default().fg(Color::Green).bg(Color::Rgb(0, 40, 0));
                        left_items.push(ListItem::new(Line::from("")));
                        let mut right_spans = vec![
                            Span::styled(marker, marker_style),
                            Span::styled(new_no, line_num_style),
                        ];
                        right_spans.extend(build_highlighted_spans("+", &line.content, &line.highlights, content_style, is_current));
                        right_items.push(ListItem::new(Line::from(right_spans)));
                    }
                    LineKind::Context => {
                        let old_no = line.old_line_no.map_or("    ".to_string(), |n| format!("{:>4}", n));
                        let new_no = line.new_line_no.map_or("    ".to_string(), |n| format!("{:>4}", n));
                        let content_style = Style::default().fg(Color::White);
                        let mut left_spans = vec![
                            Span::styled(marker, marker_style),
                            Span::styled(old_no, line_num_style),
                        ];
                        left_spans.extend(build_highlighted_spans(" ", &line.content, &line.highlights, content_style, is_current));
                        left_items.push(ListItem::new(Line::from(left_spans)));

                        let mut right_spans = vec![
                            Span::styled(marker, marker_style),
                            Span::styled(new_no, line_num_style),
                        ];
                        right_spans.extend(build_highlighted_spans(" ", &line.content, &line.highlights, content_style, is_current));
                        right_items.push(ListItem::new(Line::from(right_spans)));
                    }
                }
            }
            DisplayLine::Annotation { annotation, .. } => {
                // Annotation in a prominent box - handle multiple lines
                let icon = match annotation.annotation_type {
                    AnnotationType::Comment => "💬",
                    AnnotationType::Todo => "📋",
                };
                let style = Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(50, 80, 80));

                // Split content by newlines and create multiple lines
                let lines: Vec<Line> = annotation.content
                    .lines()
                    .enumerate()
                    .map(|(i, line)| {
                        let prefix = if i == 0 {
                            format!("   {} ", icon)
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

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    match &app.mode {
        Mode::Normal => {
            let content = if let Some(msg) = &app.message {
                msg.clone()
            } else if app.expanded_file.is_some() {
                " j/k:line  n/N:chunk  ^d/^u:page  x:collapse  a:annotate  ?:help  q:quit".to_string()
            } else if app.search.is_some() {
                " n/N:match  Esc:clear  j/k:nav  Tab:file  x:expand  a:annotate  ?:help  q:quit".to_string()
            } else {
                " j/k:nav  n/N:chunk  Tab:file  f:find  /:search  x:expand  a:annotate  ?:help  q:quit".to_string()
            };
            let status = Paragraph::new(content)
                .style(Style::default().fg(Color::DarkGray).bg(Color::Rgb(30, 30, 30)));
            f.render_widget(status, area);
        }
        Mode::AddAnnotation | Mode::EditAnnotation(_) => {
            // Multi-line input area with border
            let title = match &app.mode {
                Mode::AddAnnotation => format!(" Add {} (^J: newline, Enter: save, Esc: cancel) ", app.annotation_type.as_str()),
                Mode::EditAnnotation(_) => " Edit (^J: newline, Enter: save, Esc: cancel) ".to_string(),
                _ => String::new(),
            };

            let input_with_cursor = format!("{}_", app.annotation_input);

            let input = Paragraph::new(input_with_cursor)
                .style(Style::default().fg(Color::White).bg(Color::Rgb(40, 40, 50)))
                .block(Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
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
                .style(Style::default().fg(Color::Yellow).bg(Color::Rgb(40, 40, 50)));
            f.render_widget(status, area);
        }
    }
}

fn render_help(f: &mut Frame) {
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
        "    c         Toggle annotation visibility",
        "    s         Toggle side-by-side view",
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
        "    Esc       Cancel",
        "",
    ];

    let help = Paragraph::new(help_text.join("\n"))
        .style(Style::default())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Help ")
                .style(Style::default().bg(Color::DarkGray)),
        )
        .wrap(Wrap { trim: false });

    f.render_widget(Clear, area);
    f.render_widget(help, area);
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
fn token_color(token_type: TokenType) -> Color {
    match token_type {
        TokenType::Keyword => Color::Magenta,
        TokenType::String => Color::Yellow,
        TokenType::Comment => Color::DarkGray,
        TokenType::Function => Color::Blue,
        TokenType::Type => Color::Cyan,
        TokenType::Number => Color::LightRed,
        TokenType::Operator => Color::White,
        TokenType::Variable => Color::LightCyan,
        TokenType::Atom => Color::Cyan,
        TokenType::Module => Color::LightYellow,
        TokenType::Default => Color::White,
    }
}

/// Build spans with syntax highlighting for a diff line
fn build_highlighted_spans<'a>(
    prefix: &'a str,
    content: &'a str,
    highlights: &[HighlightRange],
    base_style: Style,
    is_current: bool,
) -> Vec<Span<'a>> {
    let base_style = if is_current {
        base_style.add_modifier(Modifier::REVERSED)
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
                let color = token_color(highlight.token_type);
                let style = if is_current {
                    Style::default().fg(color).add_modifier(Modifier::REVERSED)
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
