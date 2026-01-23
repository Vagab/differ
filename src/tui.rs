//! TUI layer using ratatui and crossterm
//!
//! Provides interactive diff viewing with annotation support.

use crate::config::Config;
use crate::diff::{DiffEngine, DiffFile, DiffLine, LineKind};
use crate::storage::{Annotation, AnnotationType, Side, Storage};
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Normal,
    AddAnnotation,
    EditAnnotation(i64), // annotation id
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
                self.display_lines.push(DisplayLine::Diff {
                    line: line.clone(),
                    file_idx,
                    file_path: file_path.to_string(),
                });

                self.add_annotations_for_line(file_idx, file_path, line);
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
                    let del_line = DiffLine {
                        kind: LineKind::Deletion,
                        old_line_no: None,
                        new_line_no: None,
                        content: del_content.clone(),
                        highlights: Vec::new(),
                    };
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

            let diff_line = DiffLine {
                kind,
                old_line_no: Some(line_no),
                new_line_no: Some(line_no),
                content: content.to_string(),
                highlights: Vec::new(),
            };

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
                let del_line = DiffLine {
                    kind: LineKind::Deletion,
                    old_line_no: None,
                    new_line_no: None,
                    content: del_content.clone(),
                    highlights: Vec::new(),
                };
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
            // n/N: next/prev file in normal mode, next/prev change chunk in expanded mode
            KeyCode::Char('n') => {
                if self.expanded_file.is_some() {
                    self.next_change_chunk();
                } else {
                    self.next_file();
                }
            }
            KeyCode::Char('N') => {
                if self.expanded_file.is_some() {
                    self.prev_change_chunk();
                } else {
                    self.prev_file();
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

            // Change navigation (especially useful in expanded mode)
            KeyCode::Char(']') => self.next_change(),
            KeyCode::Char('[') => self.prev_change(),

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

    /// Jump to next change (addition or deletion) - single line
    fn next_change(&mut self) {
        let total = self.display_lines.len();
        for idx in (self.current_line_idx + 1)..total {
            if self.is_change_line(idx) {
                self.current_line_idx = idx;
                self.adjust_scroll();
                return;
            }
        }
        // Wrap around to beginning
        for idx in 0..self.current_line_idx {
            if self.is_change_line(idx) {
                self.current_line_idx = idx;
                self.adjust_scroll();
                return;
            }
        }
    }

    /// Jump to previous change (addition or deletion) - single line
    fn prev_change(&mut self) {
        for idx in (0..self.current_line_idx).rev() {
            if self.is_change_line(idx) {
                self.current_line_idx = idx;
                self.adjust_scroll();
                return;
            }
        }
        // Wrap around to end
        let total = self.display_lines.len();
        for idx in (self.current_line_idx..total).rev() {
            if self.is_change_line(idx) {
                self.current_line_idx = idx;
                self.adjust_scroll();
                return;
            }
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
    let (file_path, file_idx) = app.current_file_info().unwrap_or(("<none>".to_string(), 0));

    // Calculate file stats
    let file = app.files.get(file_idx);
    let (additions, deletions) = file.map(|f| {
        let adds: usize = f.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == LineKind::Addition).count();
        let dels: usize = f.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == LineKind::Deletion).count();
        (adds, dels)
    }).unwrap_or((0, 0));

    // Different style when expanded
    let (style, expanded_indicator) = if app.expanded_file.is_some() {
        (
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(80, 60, 40)) // Orange-ish when expanded
                .add_modifier(Modifier::BOLD),
            " [FULL FILE - x to collapse]"
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

                    let style = Style::default()
                        .fg(Color::White)
                        .bg(Color::Rgb(40, 40, 60))
                        .add_modifier(Modifier::BOLD);

                    ListItem::new(Line::from(vec![
                        Span::styled(format!(" Δ {}  ", path), style),
                        Span::styled(format!("+{} ", adds), Style::default().fg(Color::Green).bg(Color::Rgb(40, 40, 60))),
                        Span::styled(format!("-{} ", dels), Style::default().fg(Color::Red).bg(Color::Rgb(40, 40, 60))),
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

                    let content_style = if is_current {
                        content_style.add_modifier(Modifier::REVERSED)
                    } else {
                        content_style
                    };

                    ListItem::new(Line::from(vec![
                        Span::styled(annotation_marker, Style::default().fg(Color::Yellow)),
                        Span::styled(line_num, line_num_style),
                        Span::raw(" "),
                        Span::styled(format!("{} {}", prefix, line.content), content_style),
                    ]))
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

                let style = Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(40, 40, 60))
                    .add_modifier(Modifier::BOLD);

                let header = Line::from(vec![
                    Span::styled(format!(" Δ {} ", path), style),
                    Span::styled(format!("+{} ", adds), Style::default().fg(Color::Green).bg(Color::Rgb(40, 40, 60))),
                    Span::styled(format!("-{} ", dels), Style::default().fg(Color::Red).bg(Color::Rgb(40, 40, 60))),
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
                        let content_style = if is_current { content_style.add_modifier(Modifier::REVERSED) } else { content_style };
                        left_items.push(ListItem::new(Line::from(vec![
                            Span::styled(marker, marker_style),
                            Span::styled(old_no, line_num_style),
                            Span::styled(format!(" - {}", line.content), content_style),
                        ])));
                        right_items.push(ListItem::new(Line::from("")));
                    }
                    LineKind::Addition => {
                        let new_no = line.new_line_no.map_or("    ".to_string(), |n| format!("{:>4}", n));
                        let content_style = Style::default().fg(Color::Green).bg(Color::Rgb(0, 40, 0));
                        let content_style = if is_current { content_style.add_modifier(Modifier::REVERSED) } else { content_style };
                        left_items.push(ListItem::new(Line::from("")));
                        right_items.push(ListItem::new(Line::from(vec![
                            Span::styled(marker, marker_style),
                            Span::styled(new_no, line_num_style),
                            Span::styled(format!(" + {}", line.content), content_style),
                        ])));
                    }
                    LineKind::Context => {
                        let old_no = line.old_line_no.map_or("    ".to_string(), |n| format!("{:>4}", n));
                        let new_no = line.new_line_no.map_or("    ".to_string(), |n| format!("{:>4}", n));
                        let content_style = Style::default().fg(Color::White);
                        let content_style = if is_current { content_style.add_modifier(Modifier::REVERSED) } else { content_style };
                        left_items.push(ListItem::new(Line::from(vec![
                            Span::styled(marker, marker_style),
                            Span::styled(old_no, line_num_style),
                            Span::styled(format!("   {}", line.content), content_style),
                        ])));
                        right_items.push(ListItem::new(Line::from(vec![
                            Span::styled(marker, marker_style),
                            Span::styled(new_no, line_num_style),
                            Span::styled(format!("   {}", line.content), content_style),
                        ])));
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
            } else {
                " j/k:nav  ^d/^u:page  n/N:file  x:expand  a:annotate  s:side  ?:help  q:quit".to_string()
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
    }
}

fn render_help(f: &mut Frame) {
    let area = centered_rect(60, 80, f.area());

    let help_text = vec![
        "",
        "  Navigation:",
        "    j / Down  Move down line by line",
        "    k / Up    Move up line by line",
        "    n         Next file / next chunk (expanded)",
        "    N         Previous file / prev chunk (expanded)",
        "    ]         Next change (addition/deletion)",
        "    [         Previous change",
        "    Ctrl+d    Half page down",
        "    Ctrl+u    Half page up",
        "    g         Go to start",
        "    G         Go to end",
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
