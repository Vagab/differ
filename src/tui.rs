//! TUI layer using ratatui and crossterm
//!
//! Provides interactive diff viewing with annotation support.

use crate::config::{AiTarget, Config};
use crate::diff::{
    DiffEngine, DiffFile, DiffHunk, DiffLine, DiffMode, FileStatus, HighlightRange, InlineRange,
    LineKind,
};
use crate::storage::{Annotation, AnnotationType, Side, Storage};
use crate::syntax::SyntaxHighlighter;
use anyhow::{anyhow, Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers, MouseEventKind, DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use notify::{event::ModifyKind, Event as FsEventRaw, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::mem;
use std::time::{Duration, Instant};
use std::collections::VecDeque;
use tui_textarea::{CursorMove, Input, TextArea};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use similar::{ChangeTag, TextDiff};

const REATTACH_CONTEXT_LINES: usize = 2;
const STREAM_BATCH_FILES: usize = 3;
const INTRALINE_PAIR_RATIO_THRESHOLD: f32 = 0.6;
const FULL_HIGHLIGHT_MAX_INFLIGHT: usize = 2;

const HELP_TEXT: &[&str] = &[
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
    "    :         Go to line (expanded view)",
    "    G         Go to end",
    "",
    "  Search & Copy:",
    "    f         Search by filename (regex)",
    "    /         Search in content (regex)",
    "    n / N     Next / previous match (when searching)",
    "    Esc       Clear search",
    "    y         Copy selection/current line",
    "",
    "  View:",
    "    x         Expand/collapse current file (full view)",
    "    h         Toggle deletions in expanded view",
    "    c         Collapse/expand current file",
    "    B         Toggle sidebar",
    "    b         Focus sidebar",
    "    v         Toggle side-by-side view",
    "    s         Stage/unstage current hunk",
    "    D         Discard current hunk (unstaged)",
    "    u         Toggle staged/unstaged view",
    "    R         Reload diff",
    "    Ctrl+r    Reload diff (global)",
    "    @         Send annotation to AI",
    "    P         Toggle AI pane",
    "    A         Annotation list",
        "",
        "  Annotations:",
        "    a         Add annotation at current line",
        "    e         Edit annotation at current line",
        "    d         Delete annotation at current line",
        "    r         Resolve annotation at current line",
        "    t         Toggle annotation type (comment/todo)",
        "    V         Select lines for range annotation",
        "",
    "  Other:",
    "    ?         Toggle this help",
    "    q         Quit",
    "",
        "  In annotation mode:",
        "    Enter     Save annotation",
        "    Ctrl+j    Add newline",
        "    Ctrl+d/u  Scroll diff",
        "    PgUp/PgDn Scroll diff",
        "    Ctrl+t    Toggle annotation type",
        "    Arrows    Move cursor",
    "    Home/End  Line start/end",
    "    Del/BS    Delete",
    "    Esc       Cancel",
    "",
];

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
        file_idx: usize,
        hunk_idx: usize,
    },
    /// Hunk context (function/module) line
    HunkContext {
        text: String,
        line_no: u32,
        highlights: Vec<HighlightRange>,
    },
    /// End of hunk marker for spacing
    HunkEnd {
        file_idx: usize,
        hunk_idx: usize,
    },
    /// A diff line (addition, deletion, or context)
    Diff {
        line: DiffLine,
        #[allow(dead_code)]
        file_idx: usize,
        file_path: String,
        hunk_idx: Option<usize>,
    },
    /// An annotation shown inline below its line
    Annotation {
        annotation: Annotation,
        #[allow(dead_code)]
        file_idx: usize,
        orphaned: bool,
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
    diff_mode: DiffMode,
    diff_paths: Vec<String>,

    // Diff state
    files: Vec<DiffFile>,
    diff_file_index: HashMap<String, usize>,
    display_lines: Vec<DisplayLine>,
    file_line_ranges: Vec<Option<(usize, usize)>>,
    current_line_idx: usize,
    scroll_offset: usize,

    // All annotations keyed by (file_path, side, line_no)
    all_annotations: Vec<Annotation>,

    // Syntax highlighting
    syntax_highlighter: SyntaxHighlighter,
    syntax_cache_old: HashMap<String, Vec<Vec<HighlightRange>>>,
    syntax_cache_new: HashMap<String, Vec<Vec<HighlightRange>>>,
    expanded_highlight_files: HashSet<String>,
    full_highlight_pending: HashSet<String>,
    full_highlight_queue: VecDeque<String>,
    full_highlight_inflight: usize,
    full_highlight_rx: Receiver<HighlightEvent>,
    full_highlight_tx: Sender<HighlightEvent>,
    diff_generation: u64,
    diff_loading: bool,
    diff_rx: Receiver<DiffStreamEvent>,
    diff_tx: Sender<DiffStreamEvent>,
    diff_target: Option<(String, u32)>,
    diff_pending_reset: bool,
    cached_unstaged: Option<Vec<DiffFile>>,
    cached_staged: Option<Vec<DiffFile>>,
    pending_stream_files: Vec<DiffFile>,
    fs_rx: Receiver<FsEvent>,
    fs_tx: Sender<FsEvent>,
    fs_pending: bool,
    fs_last_event: Option<Instant>,

    // UI state
    mode: Mode,
    annotation_input: TextArea<'static>,
    annotation_type: AnnotationType,
    annotation_range: Option<(u32, u32)>,
    annotation_side: Option<Side>,
    message: Option<String>,
    show_help: bool,
    help_scroll: usize,
    help_visible_height: usize,
    show_annotations: bool,
    side_by_side: bool,
    visible_height: usize,
    content_width: usize,
    show_old_in_expanded: bool,
    sidebar_open: bool,
    sidebar_focused: bool,
    sidebar_index: usize,
    sidebar_scroll: usize,
    expanded_file: Option<usize>, // When Some, only show this file
    collapsed_files: HashSet<String>,
    collapsed_files_unstaged: HashSet<String>,
    collapsed_files_staged: HashSet<String>,
    collapsed_files_other: HashSet<String>,
    selection_active: bool,
    selection_start: Option<usize>,
    selection_file_idx: Option<usize>,
    selection_hunk_idx: Option<usize>,
    // Position to restore when collapsing expanded view
    pre_expand_position: Option<(usize, usize)>, // (line_idx, scroll_offset)
    pre_expand_display_lines: Option<Vec<DisplayLine>>,
    pre_expand_file_line_ranges: Option<Vec<Option<(usize, usize)>>>,
    pre_expand_diff_generation: Option<u64>,
    annotation_list_idx: usize,
    goto_line_input: String,

    // Search state
    search: Option<SearchState>,
    ai_jobs: Vec<AiJob>,
    ai_next_id: u64,
    ai_rx: Receiver<AiEvent>,
    ai_tx: Sender<AiEvent>,
    show_ai_pane: bool,
    ai_selected_idx: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Normal,
    AddAnnotation,
    EditAnnotation(i64), // annotation id
    SearchFile,          // searching by filename
    SearchContent,       // searching within content
    AnnotationList,
    GotoLine,
}

#[derive(Debug)]
enum DiffStreamEvent {
    File { generation: u64, file: DiffFile },
    Done { generation: u64, error: Option<String> },
}

#[derive(Debug, Clone, Copy)]
enum FsEvent {
    Changed,
}

#[derive(Debug, Clone)]
enum AiStatus {
    Running,
    Done { ok: bool },
}

#[derive(Debug, Clone)]
enum HighlightEvent {
    FullFile {
        generation: u64,
        file_key: String,
        map: Vec<Vec<HighlightRange>>,
    },
}

#[derive(Debug, Clone)]
struct AiJob {
    id: u64,
    annotation_id: i64,
    annotation_type: AnnotationType,
    file_path: String,
    line: u32,
    status: AiStatus,
    output: String,
    target: AiTarget,
}

#[derive(Debug)]
enum AiEvent {
    Output { job_id: u64, chunk: String },
    Done { job_id: u64, ok: bool },
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
    hunk_ctx_bg: Color,
    hunk_ctx_fg: Color,
    hunk_border: Color,
    selection_bg: Color,
    added_bg: Color,
    added_fg: Color,
    deleted_bg: Color,
    deleted_fg: Color,
    intraline_added_bg: Color,
    intraline_deleted_bg: Color,
    context_fg: Color,
    line_num: Color,
    annotation_bg: Color,
    annotation_fg: Color,
    todo_bg: Color,
    todo_fg: Color,
    resolved_bg: Color,
    resolved_fg: Color,
    annotation_marker: Color,
    status_bg: Color,
    status_fg: Color,
    search_bg: Color,
    search_fg: Color,
    help_bg: Color,
    help_fg: Color,
    border: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
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
            hunk_bg: Color::Rgb(54, 62, 92),
            hunk_fg: Color::Rgb(220, 228, 245),
            hunk_ctx_bg: Color::Rgb(36, 44, 70),
            hunk_ctx_fg: Color::Rgb(185, 205, 235),
            hunk_border: Color::Rgb(80, 88, 72),
            selection_bg: Color::Rgb(56, 66, 88),
            added_bg: Color::Rgb(20, 40, 28),
            added_fg: Color::Rgb(168, 234, 196),
            deleted_bg: Color::Rgb(48, 26, 26),
            deleted_fg: Color::Rgb(238, 170, 170),
            intraline_added_bg: Color::Rgb(28, 66, 38),
            intraline_deleted_bg: Color::Rgb(84, 34, 34),
            context_fg: Color::Rgb(220, 224, 230),
            line_num: Color::Rgb(120, 130, 140),
            annotation_bg: Color::Rgb(40, 76, 78),
            annotation_fg: Color::Rgb(230, 245, 245),
            todo_bg: Color::Rgb(115, 86, 26),
            todo_fg: Color::Rgb(255, 245, 210),
            resolved_bg: Color::Rgb(34, 40, 42),
            resolved_fg: Color::Rgb(160, 170, 180),
            annotation_marker: Color::Rgb(255, 208, 96),
            status_bg: Color::Rgb(22, 24, 28),
            status_fg: Color::Rgb(150, 160, 170),
            search_bg: Color::Rgb(40, 44, 56),
            search_fg: Color::Rgb(255, 225, 140),
            help_bg: Color::Rgb(30, 34, 42),
            help_fg: Color::Rgb(220, 230, 240),
            border: Color::Rgb(210, 175, 90),
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
        diff_mode: DiffMode,
        diff_paths: Vec<String>,
    ) -> Result<Self> {
        let show_annotations = config.show_annotations;
        let side_by_side = config.side_by_side;
        let (ai_tx, ai_rx) = mpsc::channel();
        let (full_highlight_tx, full_highlight_rx) = mpsc::channel();
        let (diff_tx, diff_rx) = mpsc::channel();
        let (fs_tx, fs_rx) = mpsc::channel();

        let syntax_theme = config.syntax_theme.clone();

        Ok(Self {
            storage,
            diff_engine,
            repo_path,
            repo_id,
            config,
            diff_mode,
            diff_paths,
            files,
            diff_file_index: HashMap::new(),
            display_lines: Vec::new(),
            file_line_ranges: Vec::new(),
            current_line_idx: 0,
            scroll_offset: 0,
            all_annotations: Vec::new(),
            syntax_highlighter: SyntaxHighlighter::new(syntax_theme.as_deref())?,
            syntax_cache_old: HashMap::new(),
            syntax_cache_new: HashMap::new(),
            expanded_highlight_files: HashSet::new(),
            full_highlight_pending: HashSet::new(),
            full_highlight_queue: VecDeque::new(),
            full_highlight_inflight: 0,
            full_highlight_rx,
            full_highlight_tx,
            diff_generation: 0,
            diff_loading: false,
            diff_rx,
            diff_tx,
            diff_target: None,
            diff_pending_reset: false,
            cached_unstaged: None,
            cached_staged: None,
            pending_stream_files: Vec::new(),
            fs_rx,
            fs_tx,
            fs_pending: false,
            fs_last_event: None,
            mode: Mode::Normal,
            annotation_input: TextArea::default(),
            annotation_type: AnnotationType::Comment,
            annotation_range: None,
            annotation_side: None,
            message: None,
            show_help: false,
            help_scroll: 0,
            help_visible_height: 0,
            show_annotations,
            side_by_side,
            visible_height: 20, // Will be updated on first render
            content_width: 80,
            show_old_in_expanded: true,
            sidebar_open: false,
            sidebar_focused: false,
            sidebar_index: 0,
            sidebar_scroll: 0,
            expanded_file: None,
            collapsed_files: HashSet::new(),
            collapsed_files_unstaged: HashSet::new(),
            collapsed_files_staged: HashSet::new(),
            collapsed_files_other: HashSet::new(),
            selection_active: false,
            selection_start: None,
            selection_file_idx: None,
            selection_hunk_idx: None,
            pre_expand_position: None,
            pre_expand_display_lines: None,
            pre_expand_file_line_ranges: None,
            pre_expand_diff_generation: None,
            search: None,
            annotation_list_idx: 0,
            goto_line_input: String::new(),
            ai_jobs: Vec::new(),
            ai_next_id: 1,
            ai_rx,
            ai_tx,
            show_ai_pane: false,
            ai_selected_idx: 0,
        })
    }

    /// Load all annotations for all files and build the display lines
    fn load_all_annotations(&mut self) -> Result<()> {
        self.all_annotations = self.storage.list_annotations(self.repo_id, None)?;
        Ok(())
    }

    fn file_highlight_keys(file: &DiffFile) -> (String, String, String) {
        let file_key = file
            .new_path
            .as_ref()
            .or(file.old_path.as_ref())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        let old_key = file
            .old_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let new_key = file
            .new_path
            .as_ref()
            .or(file.old_path.as_ref())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        (file_key, old_key, new_key)
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

    /// Build the unified display lines from all files
    fn build_display_lines(&mut self) {
        self.display_lines.clear();
        self.file_line_ranges = vec![None; self.files.len()];

        // Clone files to avoid borrow issues
        let files = self.files.clone();
        let expanded_file = self.expanded_file;
        let mut has_any = false;

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

            let include_spacer = has_any && expanded_file.is_none();
            let file_lines =
                self.build_file_lines(file_idx, &file, &file_path, include_spacer);
            let start = self.display_lines.len();
            self.display_lines.extend(file_lines);
            let len = self.display_lines.len().saturating_sub(start);
            if len > 0 {
                self.file_line_ranges[file_idx] = Some((start, len));
                has_any = true;
            }
        }
    }

    fn build_file_lines(
        &mut self,
        file_idx: usize,
        file: &DiffFile,
        file_path: &str,
        include_spacer: bool,
    ) -> Vec<DisplayLine> {
        let mut lines = Vec::new();

        if include_spacer {
            lines.push(DisplayLine::Spacer);
            lines.push(DisplayLine::Spacer);
        }

        lines.push(DisplayLine::FileHeader {
            path: file_path.to_string(),
            file_idx,
        });

        if self.collapsed_files.contains(file_path) && self.expanded_file.is_none() {
            return lines;
        }

        if self.expanded_file.is_some() {
            self.build_expanded_file_lines_into(&mut lines, file_idx, file, file_path);
        } else {
            self.build_diff_hunk_lines_into(&mut lines, file_idx, file, file_path);
            self.maybe_schedule_full_file_highlight(file_idx, file_path);
        }

        lines
    }

    fn git_show(&self, spec: &str) -> Option<String> {
        let output = Command::new("git")
            .arg("show")
            .arg(spec)
            .current_dir(&self.repo_path)
            .output()
            .ok()?;
        if output.status.success() {
            Some(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            None
        }
    }

    fn read_working_file(&self, path: &str) -> Option<String> {
        let full_path = self.repo_path.join(path);
        std::fs::read(&full_path)
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
    }

    fn read_file_lines(&self, file_path: &str) -> Option<Vec<String>> {
        let full_path = self.repo_path.join(file_path);
        let content = std::fs::read(&full_path).ok()?;
        let text = String::from_utf8_lossy(&content);
        Some(text.lines().map(|l| l.to_string()).collect())
    }

    fn git_merge_base(&self, from: &str, to: &str) -> Option<String> {
        let output = Command::new("git")
            .arg("merge-base")
            .arg(from)
            .arg(to)
            .current_dir(&self.repo_path)
            .output()
            .ok()?;
        if output.status.success() {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            None
        }
    }

    fn build_highlight_map(&mut self, content: &str, file_path: &str) -> Vec<Vec<HighlightRange>> {
        let per_line = self.syntax_highlighter.highlight_file(content, file_path);
        per_line
            .into_iter()
            .map(|line_hl| {
                line_hl
                    .into_iter()
                    .map(|h| HighlightRange {
                        start: h.start,
                        end: h.end,
                        style: h.style,
                    })
                    .collect()
            })
            .collect()
    }

    fn highlight_hunk_lines(
        &mut self,
        file_path: &str,
        lines: &[DiffLine],
    ) -> (Vec<Vec<HighlightRange>>, bool) {
        if !self.config.syntax_highlighting {
            return (vec![Vec::new(); lines.len()], false);
        }
        let line_refs: Vec<&str> = lines.iter().map(|line| line.content.as_str()).collect();
        let (per_line, ends_in_string) =
            self.syntax_highlighter
                .highlight_lines_with_string_state(&line_refs, file_path);
        let converted = per_line
            .into_iter()
            .map(|line_hl| {
                line_hl
                    .into_iter()
                    .map(|h| HighlightRange {
                        start: h.start,
                        end: h.end,
                        style: h.style,
                    })
                    .collect()
            })
            .collect();
        (converted, ends_in_string)
    }

    fn maybe_schedule_full_file_highlight(&mut self, _file_idx: usize, file_path: &str) {
        if !self.config.syntax_highlighting {
            return;
        }
        if self.syntax_cache_new.contains_key(file_path) {
            return;
        }
        if self.expanded_highlight_files.contains(file_path) {
            return;
        }
        if !self
            .full_highlight_pending
            .insert(file_path.to_string())
        {
            return;
        }
        if let Some((current_path, _)) = self.current_file_info() {
            if current_path == file_path {
                self.full_highlight_queue
                    .push_front(file_path.to_string());
            } else {
                self.full_highlight_queue.push_back(file_path.to_string());
            }
        } else {
            self.full_highlight_queue.push_back(file_path.to_string());
        }
        self.kick_full_highlight_jobs();
    }

    fn kick_full_highlight_jobs(&mut self) {
        while self.full_highlight_inflight < FULL_HIGHLIGHT_MAX_INFLIGHT {
            let Some(file_key) = self.full_highlight_queue.pop_front() else {
                break;
            };
            if !self.full_highlight_pending.contains(&file_key) {
                continue;
            }
            let Some(idx) = self.diff_file_index.get(&file_key).copied() else {
                self.full_highlight_pending.remove(&file_key);
                continue;
            };
            let Some(file) = self.files.get(idx).cloned() else {
                self.full_highlight_pending.remove(&file_key);
                continue;
            };

            self.full_highlight_inflight += 1;
            let repo_path = self.repo_path.clone();
            let diff_mode = self.diff_mode.clone();
            let theme_name = self.config.syntax_theme.clone();
            let tx = self.full_highlight_tx.clone();
            let generation = self.diff_generation;
            let file_key_clone = file_key.clone();

            thread::spawn(move || {
                let mut highlighter = match SyntaxHighlighter::new(theme_name.as_deref()) {
                    Ok(h) => h,
                    Err(_) => return,
                };
                let content =
                    load_new_file_content_for_highlight(&repo_path, &diff_mode, &file, &file_key_clone);
                let map = content
                    .as_ref()
                    .map(|c| build_highlight_map_with(&mut highlighter, c, &file_key_clone))
                    .unwrap_or_default();
                let _ = tx.send(HighlightEvent::FullFile {
                    generation,
                    file_key: file_key_clone,
                    map,
                });
            });
        }
    }

    fn load_highlight_maps(
        &mut self,
        file: &DiffFile,
        old_path: Option<&str>,
        new_path: Option<&str>,
        full_file: bool,
    ) -> (Vec<Vec<HighlightRange>>, Vec<Vec<HighlightRange>>) {
        if !self.config.syntax_highlighting {
            return (Vec::new(), Vec::new());
        }

        let mut need_old = matches!(file.status, FileStatus::Deleted);
        let mut need_new = matches!(file.status, FileStatus::Added);
        if full_file {
            need_old = !matches!(file.status, FileStatus::Added) && old_path.is_some();
            need_new = !matches!(file.status, FileStatus::Deleted) && new_path.or(old_path).is_some();
        } else if !need_old || !need_new {
            for hunk in &file.hunks {
                for line in &hunk.lines {
                    match line.kind {
                        LineKind::Deletion => need_old = true,
                        LineKind::Addition | LineKind::Context => need_new = true,
                    }
                    if need_old && need_new {
                        break;
                    }
                }
                if need_old && need_new {
                    break;
                }
            }
        }

        let (mut old_content, mut new_content) = (None, None);
        match self.diff_mode {
            DiffMode::Unstaged => {
                if need_old {
                    if let Some(path) = old_path {
                        old_content = self.git_show(&format!(":{}", path));
                    }
                }
                if need_new {
                    if let Some(path) = new_path {
                        new_content = self.read_working_file(path);
                    }
                }
            }
            DiffMode::Staged => {
                if need_old {
                    if let Some(path) = old_path {
                        old_content = self.git_show(&format!("HEAD:{}", path));
                    }
                }
                if need_new {
                    if let Some(path) = new_path {
                        new_content = self.git_show(&format!(":{}", path));
                    }
                }
            }
            DiffMode::WorkingTree { ref base } => {
                if need_old {
                    if let Some(path) = old_path {
                        old_content = self.git_show(&format!("{}:{}", base, path));
                    }
                }
                if need_new {
                    if let Some(path) = new_path {
                        new_content = self.read_working_file(path);
                    }
                }
            }
            DiffMode::Commits { ref from, ref to } => {
                if need_old {
                    if let Some(path) = old_path {
                        old_content = self.git_show(&format!("{}:{}", from, path));
                    }
                }
                if need_new {
                    if let Some(path) = new_path {
                        new_content = self.git_show(&format!("{}:{}", to, path));
                    }
                }
            }
            DiffMode::MergeBase { ref from, ref to } => {
                let base = self.git_merge_base(from, to);
                if need_old {
                    if let (Some(path), Some(base)) = (old_path, base.as_deref()) {
                        old_content = self.git_show(&format!("{}:{}", base, path));
                    }
                }
                if need_new {
                    if let Some(path) = new_path {
                        new_content = self.git_show(&format!("{}:{}", to, path));
                    }
                }
            }
            _ => {
                if need_new {
                    if let Some(path) = new_path.or(old_path) {
                        new_content = self.read_working_file(path);
                    }
                }
            }
        }

        if file.status == FileStatus::Added {
            old_content = None;
        }
        if file.status == FileStatus::Deleted {
            new_content = None;
        }

        let old_map = old_content
            .as_ref()
            .map(|c| self.build_highlight_map(c, old_path.unwrap_or("")))
            .unwrap_or_default();
        let new_map = new_content
            .as_ref()
            .map(|c| self.build_highlight_map(c, new_path.or(old_path).unwrap_or("")))
            .unwrap_or_default();

        (old_map, new_map)
    }

    /// Build display lines for diff hunks (normal mode)
    fn build_diff_hunk_lines_into(
        &mut self,
        out: &mut Vec<DisplayLine>,
        file_idx: usize,
        file: &DiffFile,
        file_path: &str,
    ) {
        let full_map = self.syntax_cache_new.get(file_path).cloned();
        let mut needs_full_highlight = false;

        for (hunk_idx, hunk) in file.hunks.iter().enumerate() {
            let inline_ranges = if self.diff_loading {
                vec![Vec::new(); hunk.lines.len()]
            } else {
                compute_intraline_ranges(&hunk.lines)
            };
            let (hunk_highlights, hunk_needs_full) = self.highlight_hunk_lines(file_path, &hunk.lines);
            if hunk_needs_full {
                needs_full_highlight = true;
            }
            // Count additions and deletions in this hunk
            let additions = hunk.lines.iter().filter(|l| l.kind == LineKind::Addition).count();
            let deletions = hunk.lines.iter().filter(|l| l.kind == LineKind::Deletion).count();

            // Add spacing before hunk
            out.push(DisplayLine::Spacer);

            if let Some(text) = hunk.header.as_ref().filter(|t| !t.trim().is_empty()) {
                let clean = text.trim().trim_start_matches("@@").trim().to_string();
                let highlights = if self.config.syntax_highlighting {
                    let mut lines = self.syntax_highlighter.highlight_file(
                        &format!("{}\n", clean),
                        file_path,
                    );
                    let line_hls = lines.pop().unwrap_or_default();
                    line_hls
                        .into_iter()
                        .map(|h| HighlightRange {
                            start: h.start,
                            end: h.end,
                            style: h.style,
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                out.push(DisplayLine::HunkContext {
                    text: clean,
                    line_no: hunk.new_start,
                    highlights,
                });
            }

            // Add hunk header
            let line_no = hunk.new_start;
            out.push(DisplayLine::HunkHeader {
                line_no,
                additions,
                deletions,
                file_idx,
                hunk_idx,
            });

            for (line_idx, line) in hunk.lines.iter().enumerate() {
                let mut highlighted_line = line.clone();
                if let Some(ranges) = inline_ranges.get(line_idx) {
                    highlighted_line.inline_ranges = ranges.clone();
                }
                let mut applied = false;
                if let (Some(map), Some(line_no)) = (full_map.as_ref(), highlighted_line.new_line_no)
                {
                    if let Some(line_hl) = map.get(line_no.saturating_sub(1) as usize) {
                        highlighted_line.highlights = line_hl.clone();
                        applied = true;
                    }
                }
                if !applied {
                    if let Some(ranges) = hunk_highlights.get(line_idx) {
                        highlighted_line.highlights = ranges.clone();
                    }
                }

                out.push(DisplayLine::Diff {
                    line: highlighted_line.clone(),
                    file_idx,
                    file_path: file_path.to_string(),
                    hunk_idx: Some(hunk_idx),
                });

                self.add_annotations_for_line(out, file_idx, file_path, &highlighted_line);
            }

            // Add hunk end marker
            out.push(DisplayLine::HunkEnd { file_idx, hunk_idx });
        }

        if needs_full_highlight {
            self.maybe_schedule_full_file_highlight(file_idx, file_path);
        }
    }

    /// Build display lines for full file view (expanded mode)
    fn build_expanded_file_lines_into(
        &mut self,
        out: &mut Vec<DisplayLine>,
        file_idx: usize,
        file: &DiffFile,
        file_path: &str,
    ) {
        use std::collections::HashMap;

        // Build maps of line changes from diff hunks
        // Key: new_line_no for additions/context, old_line_no for deletions
        let mut additions: HashMap<u32, String> = HashMap::new();
        let mut inline_additions: HashMap<u32, Vec<InlineRange>> = HashMap::new();
        let mut deletions: Vec<(u32, u32, String)> = Vec::new(); // (insert_after_new_line, old_line_no, content)

        let mut hunk_ranges: Vec<(std::ops::RangeInclusive<u32>, std::ops::RangeInclusive<u32>)> =
            Vec::new();
        for hunk in &file.hunks {
            let inline_ranges = if self.diff_loading {
                vec![Vec::new(); hunk.lines.len()]
            } else {
                compute_intraline_ranges(&hunk.lines)
            };
            let old_range = hunk.old_start..=hunk.old_start.saturating_add(hunk.old_lines.saturating_sub(1));
            let new_range = hunk.new_start..=hunk.new_start.saturating_add(hunk.new_lines.saturating_sub(1));
            hunk_ranges.push((old_range, new_range));
            let mut last_new_line: u32 = hunk.new_start.saturating_sub(1);
            for (line_idx, line) in hunk.lines.iter().enumerate() {
                match line.kind {
                    LineKind::Addition => {
                        if let Some(n) = line.new_line_no {
                            additions.insert(n, line.content.clone());
                            last_new_line = n;
                            if let Some(ranges) = inline_ranges.get(line_idx) {
                                if !ranges.is_empty() {
                                    inline_additions.insert(n, ranges.clone());
                                }
                            }
                        }
                    }
                    LineKind::Deletion => {
                        if let Some(old_no) = line.old_line_no {
                            deletions.push((last_new_line, old_no, line.content.clone()));
                        }
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
        let (file_key, old_key, _new_key) = Self::file_highlight_keys(file);
        let old_path = file.old_path.as_ref().map(|p| p.to_string_lossy().to_string());
        let new_path = file.new_path.as_ref().map(|p| p.to_string_lossy().to_string());

        let new_map = if self.config.syntax_highlighting {
            if !self.expanded_highlight_files.contains(&file_key) {
                let map = self.build_highlight_map(&file_content, file_path);
                self.syntax_cache_new.insert(file_key.clone(), map.clone());
                self.expanded_highlight_files.insert(file_key.clone());
                map
            } else {
                self.syntax_cache_new
                    .get(&file_key)
                    .cloned()
                    .unwrap_or_default()
            }
        } else {
            Vec::new()
        };

        let mut old_map = Vec::new();
        if self.config.syntax_highlighting && !deletions.is_empty() && !old_key.is_empty() {
            if let Some(cached) = self.syntax_cache_old.get(&old_key).cloned() {
                old_map = cached;
            } else {
                let (computed_old, _computed_new) =
                    self.load_highlight_maps(file, old_path.as_deref(), new_path.as_deref(), true);
                self.syntax_cache_old.insert(old_key.clone(), computed_old.clone());
                old_map = computed_old;
            }
        }

        // Show all lines with changes highlighted
        for (idx, content) in file_lines.iter().enumerate() {
            let line_no = (idx + 1) as u32;

            if self.show_old_in_expanded {
                // First, show any deletions that come before this line
                for (insert_after, old_no, del_content) in &deletions {
                    let effective_insert_after = if *insert_after == 0 { 1 } else { *insert_after };
                    if effective_insert_after == line_no.saturating_sub(1) {
                        let mut del_line = DiffLine {
                            kind: LineKind::Deletion,
                            old_line_no: Some(*old_no),
                            new_line_no: None,
                            content: del_content.clone(),
                            highlights: Vec::new(),
                            inline_ranges: Vec::new(),
                        };
                        self.highlight_from_maps(&mut del_line, &old_map, &[]);
                        let hunk_idx = hunk_ranges.iter().position(|(old_range, _)| {
                            old_range.contains(old_no)
                        });
                        out.push(DisplayLine::Diff {
                            line: del_line,
                            file_idx,
                            file_path: file_path.to_string(),
                            hunk_idx,
                        });
                    }
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
                inline_ranges: Vec::new(),
            };
            if let Some(ranges) = inline_additions.get(&line_no) {
                diff_line.inline_ranges = ranges.clone();
            }
            if let Some(line_hl) = new_map.get(idx) {
                diff_line.highlights = line_hl
                    .iter()
                    .map(|h| HighlightRange {
                        start: h.start,
                        end: h.end,
                        style: h.style,
                    })
                    .collect();
            }

            let hunk_idx = hunk_ranges.iter().position(|(_, new_range)| {
                new_range.contains(&line_no)
            });
            out.push(DisplayLine::Diff {
                line: diff_line.clone(),
                file_idx,
                file_path: file_path.to_string(),
                hunk_idx,
            });

            self.add_annotations_for_line(out, file_idx, file_path, &diff_line);
        }

        if self.show_old_in_expanded {
            // Show any trailing deletions
            let last_line = file_lines.len() as u32;
            for (insert_after, old_no, del_content) in &deletions {
                let effective_insert_after = if *insert_after == 0 { 1 } else { *insert_after };
                if effective_insert_after >= last_line {
                    let mut del_line = DiffLine {
                        kind: LineKind::Deletion,
                        old_line_no: Some(*old_no),
                        new_line_no: None,
                        content: del_content.clone(),
                        highlights: Vec::new(),
                        inline_ranges: Vec::new(),
                    };
                    self.highlight_from_maps(&mut del_line, &old_map, &[]);
                    let hunk_idx = hunk_ranges.iter().position(|(old_range, _)| {
                        old_range.contains(old_no)
                    });
                    out.push(DisplayLine::Diff {
                        line: del_line,
                        file_idx,
                        file_path: file_path.to_string(),
                        hunk_idx,
                    });
                }
            }
        }
    }

    /// Add annotations for a diff line
    fn add_annotations_for_line(
        &mut self,
        out: &mut Vec<DisplayLine>,
        file_idx: usize,
        file_path: &str,
        line: &DiffLine,
    ) {
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
                if annotation.start_line != line_no {
                    continue;
                }
                let orphaned = annotation.side == Side::New
                    && !annotation.anchor_text.is_empty()
                    && annotation.anchor_text.trim() != line.content.trim();
                out.push(DisplayLine::Annotation {
                    annotation: annotation.clone(),
                    file_idx,
                    orphaned,
                });
            }
        }
    }

    fn annotation_marker_for_line(
        &self,
        file_path: &str,
        side: Side,
        line_no: u32,
    ) -> Option<char> {
        let mut has_range = false;
        for a in &self.all_annotations {
            if a.file_path != file_path || a.side != side {
                continue;
            }
            if a.start_line <= line_no && a.end_line.map_or(a.start_line == line_no, |e| e >= line_no) {
                if a.start_line == line_no {
                    return Some('●');
                }
                has_range = true;
            }
        }
        if has_range {
            Some('│')
        } else {
            None
        }
    }

    fn selection_range(&self) -> Option<(usize, usize)> {
        if !self.selection_active {
            return None;
        }
        let start = self.selection_start?;
        let end = self.current_line_idx;
        Some((start.min(end), start.max(end)))
    }

    fn selection_range_for_new_side(&self) -> Option<(String, u32, u32)> {
        let (start_idx, end_idx) = self.selection_range()?;
        let file_idx = self.selection_file_idx?;
        let file = self.files.get(file_idx)?;
        let file_path = file
            .new_path
            .as_ref()
            .or(file.old_path.as_ref())
            .map(|p| p.to_string_lossy().to_string())?;

        let mut start_line: Option<u32> = None;
        let mut end_line: Option<u32> = None;

        for idx in start_idx..=end_idx {
            if let Some(DisplayLine::Diff { line, .. }) = self.display_lines.get(idx) {
                if let Some(line_no) = line.new_line_no {
                    start_line = Some(start_line.map_or(line_no, |s| s.min(line_no)));
                    end_line = Some(end_line.map_or(line_no, |e| e.max(line_no)));
                }
            }
        }

        match (start_line, end_line) {
            (Some(start), Some(end)) => Some((file_path, start, end)),
            _ => None,
        }
    }

    fn selected_text_for_copy(&self) -> Option<String> {
        let (start_idx, end_idx) = self.selection_range()?;
        let mut lines: Vec<String> = Vec::new();
        for idx in start_idx..=end_idx {
            if let Some(DisplayLine::Diff { line, .. }) = self.display_lines.get(idx) {
                lines.push(line.content.clone());
            }
        }
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }

    fn copy_to_clipboard(&self, text: &str) -> Result<()> {
        let mut child = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .spawn()
            .context("Failed to start pbcopy")?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        let status = child.wait()?;
        if status.success() {
            Ok(())
        } else {
            Err(anyhow!("pbcopy failed"))
        }
    }

    fn find_line_in_expanded(&self, line_no: u32) -> Option<usize> {
        if self.expanded_file.is_none() {
            return None;
        }
        for (idx, line) in self.display_lines.iter().enumerate() {
            if let DisplayLine::Diff { line, .. } = line {
                if line.new_line_no == Some(line_no) {
                    return Some(idx);
                }
            }
        }
        None
    }

    fn find_old_line_in_expanded(&self, line_no: u32) -> Option<usize> {
        if self.expanded_file.is_none() {
            return None;
        }
        for (idx, line) in self.display_lines.iter().enumerate() {
            if let DisplayLine::Diff { line, .. } = line {
                if line.old_line_no == Some(line_no) {
                    return Some(idx);
                }
            }
        }
        None
    }

    fn highlight_from_maps(
        &self,
        line: &mut DiffLine,
        old_map: &[Vec<HighlightRange>],
        new_map: &[Vec<HighlightRange>],
    ) {
        if !self.config.syntax_highlighting {
            return;
        }

        let highlights = match line.kind {
            LineKind::Addition | LineKind::Context => {
                line.new_line_no
                    .and_then(|n| new_map.get(n.saturating_sub(1) as usize))
            }
            LineKind::Deletion => line
                .old_line_no
                .and_then(|n| old_map.get(n.saturating_sub(1) as usize)),
        };

        if let Some(line_hls) = highlights {
            line.highlights = line_hls.clone();
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

    fn file_idx_for_line(&self, idx: usize) -> Option<usize> {
        let end = idx.min(self.display_lines.len().saturating_sub(1));
        for i in (0..=end).rev() {
            if let Some(DisplayLine::FileHeader { file_idx, .. }) = self.display_lines.get(i) {
                return Some(*file_idx);
            }
        }
        None
    }

    fn hunk_idx_for_line(&self, idx: usize) -> Option<usize> {
        match self.display_lines.get(idx) {
            Some(DisplayLine::Diff { hunk_idx: Some(h), .. }) => Some(*h),
            _ => None,
        }
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
            let (start_line, end_line) = self.annotation_range.unwrap_or_else(|| {
                let line_no = line.new_line_no.or(line.old_line_no).unwrap_or(1);
                (line_no, line_no)
            });
            let side = self.annotation_side.clone().unwrap_or_else(|| {
                if line.new_line_no.is_some() {
                    Side::New
                } else {
                    Side::Old
                }
            });

            let (anchor_line, anchor_text, context_before, context_after) = if side == Side::New {
                if let Some(lines) = self.read_file_lines(&file_path) {
                    Self::build_anchor(&lines, start_line)
                } else {
                    (start_line, line.content.clone(), String::new(), String::new())
                }
            } else {
                (start_line, line.content.clone(), String::new(), String::new())
            };

            let content = self.annotation_text();
            let id = self.storage.add_annotation(
                self.repo_id,
                &file_path,
                None, // commit_sha
                side.clone(),
                start_line,
                if start_line == end_line { None } else { Some(end_line) },
                self.annotation_type.clone(),
                &content,
                anchor_line,
                &anchor_text,
                &context_before,
                &context_after,
            )?;

            self.message = Some("Annotation added".to_string());
            self.invalidate_pre_expand_cache();
            self.all_annotations.push(Annotation {
                id,
                repo_id: self.repo_id,
                file_path: file_path.clone(),
                commit_sha: None,
                side,
                start_line,
                end_line: if start_line == end_line { None } else { Some(end_line) },
                annotation_type: self.annotation_type.clone(),
                content,
                anchor_line,
                anchor_text,
                context_before,
                context_after,
                created_at: String::new(),
                resolved_at: None,
            });
            if let Some(file_idx) = self.diff_file_index.get(&file_path).copied() {
                self.update_display_lines_for_file(file_idx);
            } else {
                self.build_display_lines();
            }
        }

        self.reset_annotation_input();
        self.annotation_range = None;
        self.annotation_side = None;
        self.selection_active = false;
        self.selection_start = None;
        self.selection_file_idx = None;
        self.selection_hunk_idx = None;
        self.mode = Mode::Normal;
        Ok(())
    }

    fn edit_annotation(&mut self, id: i64) -> Result<()> {
        let content = self.annotation_text();
        self.storage
            .update_annotation(id, &content, self.annotation_type.clone())?;
        self.message = Some("Annotation updated".to_string());
        self.invalidate_pre_expand_cache();
        let mut file_path = None;
        if let Some(annotation) = self.all_annotations.iter_mut().find(|a| a.id == id) {
            annotation.content = content;
            annotation.annotation_type = self.annotation_type.clone();
            file_path = Some(annotation.file_path.clone());
        }
        if let Some(path) = file_path {
            if let Some(file_idx) = self.diff_file_index.get(&path).copied() {
                self.update_display_lines_for_file(file_idx);
            } else {
                self.build_display_lines();
            }
        }
        self.reset_annotation_input();
        self.annotation_range = None;
        self.annotation_side = None;
        self.mode = Mode::Normal;
        Ok(())
    }

    fn delete_annotation_at_line(&mut self) -> Result<()> {
        if let Some(annotation) = self.get_annotation_for_current_line() {
            let id = annotation.id;
            let file_path = annotation.file_path.clone();
            self.storage.delete_annotation(id)?;
            self.message = Some("Annotation deleted".to_string());
            self.invalidate_pre_expand_cache();
            self.all_annotations.retain(|a| a.id != id);
            if let Some(file_idx) = self.diff_file_index.get(&file_path).copied() {
                self.update_display_lines_for_file(file_idx);
            } else {
                self.build_display_lines();
            }
        }
        Ok(())
    }

    /// Check if a display line should be skipped during navigation
    fn is_skippable_line(&self, idx: usize) -> bool {
        match self.display_lines.get(idx) {
            Some(DisplayLine::FileHeader { path, .. }) => !self.collapsed_files.contains(path),
            Some(DisplayLine::Annotation { .. })
                | Some(DisplayLine::HunkHeader { .. })
                | Some(DisplayLine::HunkContext { .. })
                | Some(DisplayLine::HunkEnd { .. })
                | Some(DisplayLine::Spacer) => true,
            _ => false,
        }
    }

    fn navigate_up(&mut self) {
        if self.current_line_idx > 0 {
            let prev_idx = self.current_line_idx;
            self.current_line_idx -= 1;
            // Skip over non-navigable lines
            while self.current_line_idx > 0 && self.is_skippable_line(self.current_line_idx) {
                self.current_line_idx -= 1;
            }
            if self.selection_active {
                if let (Some(sel_file), Some(curr_file)) = (
                    self.selection_file_idx,
                    self.file_idx_for_line(self.current_line_idx),
                ) {
                    if sel_file != curr_file {
                        self.current_line_idx = prev_idx;
                    }
                }
                if self.selection_hunk_idx != self.hunk_idx_for_line(self.current_line_idx) {
                    self.current_line_idx = prev_idx;
                }
            }
            self.ensure_cursor_on_navigable();
            self.adjust_scroll();
        }
    }

    fn navigate_down(&mut self) {
        let total_lines = self.display_lines.len();
        if self.current_line_idx < total_lines.saturating_sub(1) {
            let prev_idx = self.current_line_idx;
            self.current_line_idx += 1;
            // Skip over non-navigable lines
            while self.current_line_idx < total_lines.saturating_sub(1)
                && self.is_skippable_line(self.current_line_idx)
            {
                self.current_line_idx += 1;
            }
            if self.selection_active {
                if let (Some(sel_file), Some(curr_file)) = (
                    self.selection_file_idx,
                    self.file_idx_for_line(self.current_line_idx),
                ) {
                    if sel_file != curr_file {
                        self.current_line_idx = prev_idx;
                    }
                }
                if self.selection_hunk_idx != self.hunk_idx_for_line(self.current_line_idx) {
                    self.current_line_idx = prev_idx;
                }
            }
            self.ensure_cursor_on_navigable();
            self.adjust_scroll();
        }
    }

    /// Move down by half a page (Ctrl+D)
    fn page_down(&mut self) {
        let half_page = self.visible_height / 2;
        let total_lines = self.display_lines.len();

        if self.selection_active {
            for _ in 0..half_page {
                self.navigate_down();
            }
            return;
        }

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

        if self.selection_active {
            for _ in 0..half_page {
                self.navigate_up();
            }
            return;
        }

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

    fn help_scroll_down(&mut self) {
        let max = self.max_help_scroll();
        if self.help_scroll < max {
            self.help_scroll = (self.help_scroll + 1).min(max);
        }
    }

    fn help_scroll_up(&mut self) {
        self.help_scroll = self.help_scroll.saturating_sub(1);
    }

    fn help_page_down(&mut self) {
        let max = self.max_help_scroll();
        let jump = self.help_visible_height.saturating_sub(1).max(1);
        self.help_scroll = (self.help_scroll + jump).min(max);
    }

    fn help_page_up(&mut self) {
        let jump = self.help_visible_height.saturating_sub(1).max(1);
        self.help_scroll = self.help_scroll.saturating_sub(jump);
    }

    fn max_help_scroll(&self) -> usize {
        HELP_TEXT.len().saturating_sub(self.help_visible_height.max(1))
    }

    /// Jump to next file header
    fn next_file(&mut self) {
        let current = self.current_line_idx;
        for (idx, line) in self.display_lines.iter().enumerate().skip(current + 1) {
            if let DisplayLine::FileHeader { .. } = line {
                self.current_line_idx = idx;
                self.ensure_cursor_on_navigable();
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
                    self.ensure_cursor_on_navigable();
                    self.adjust_scroll();
                    return;
                }
            }
        }
    }

    fn handle_input(&mut self, key: KeyEvent) -> Result<bool> {
        // Clear message on any input
        self.message = None;

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
            self.reload_diff()?;
            self.message = Some("Diff reloaded".to_string());
            return Ok(false);
        }

        if key.code == KeyCode::Char('?') {
            if matches!(self.mode, Mode::Normal | Mode::AnnotationList) {
                self.show_help = !self.show_help;
                if self.show_help {
                    self.help_scroll = 0;
                }
                return Ok(false);
            }
        }

        if self.show_help {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => self.help_scroll_down(),
                KeyCode::Char('k') | KeyCode::Up => self.help_scroll_up(),
                KeyCode::PageDown | KeyCode::Char(' ') => self.help_page_down(),
                KeyCode::PageUp => self.help_page_up(),
                KeyCode::Char('g') => self.help_scroll = 0,
                KeyCode::Char('G') => self.help_scroll = self.max_help_scroll(),
                KeyCode::Esc | KeyCode::Char('?') => {
                    self.show_help = false;
                }
                _ => {}
            }
            return Ok(false);
        }

        match &self.mode {
            Mode::Normal => self.handle_normal_input(key),
            Mode::AddAnnotation | Mode::EditAnnotation(_) => self.handle_annotation_input(key),
            Mode::SearchFile | Mode::SearchContent => self.handle_search_input(key),
            Mode::AnnotationList => self.handle_annotation_list_input(key),
            Mode::GotoLine => self.handle_goto_line_input(key),
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

        if key.code == KeyCode::Char('B') {
            self.sidebar_open = !self.sidebar_open;
            if self.sidebar_open {
                self.sidebar_focused = true;
                if let Some((_, file_idx)) = self.current_file_info() {
                    self.sidebar_index = file_idx.min(self.files.len().saturating_sub(1));
                } else {
                    self.sidebar_index = 0;
                }
                self.sidebar_scroll = 0;
            } else {
                self.sidebar_focused = false;
            }
            return Ok(false);
        }

        if key.code == KeyCode::Char('b') && self.sidebar_open {
            self.sidebar_focused = !self.sidebar_focused;
            return Ok(false);
        }

        if self.sidebar_open && self.sidebar_focused {
            return self.handle_sidebar_input(key);
        }

        if self.show_ai_pane {
            match key.code {
                KeyCode::Char('[') => {
                    if self.ai_selected_idx > 0 {
                        self.ai_selected_idx -= 1;
                    }
                    return Ok(false);
                }
                KeyCode::Char(']') => {
                    if self.ai_selected_idx + 1 < self.ai_jobs.len() {
                        self.ai_selected_idx += 1;
                    }
                    return Ok(false);
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Char('q') => return Ok(true), // Quit
            _ => {}
        }
        match key.code {
            KeyCode::Char('V') => {
                if self.selection_active {
                    self.selection_active = false;
                    self.selection_start = None;
                    self.selection_file_idx = None;
                    self.selection_hunk_idx = None;
                    self.message = Some("Selection cleared".to_string());
                } else if let Some(DisplayLine::Diff { line, .. }) = self.current_display_line() {
                    if line.new_line_no.is_some() {
                        self.selection_active = true;
                        self.selection_start = Some(self.current_line_idx);
                        self.selection_file_idx = self.file_idx_for_line(self.current_line_idx);
                        self.selection_hunk_idx = self.hunk_idx_for_line(self.current_line_idx);
                        self.message = Some("Selecting lines".to_string());
                    } else {
                        self.message = Some("Selection only works on new/context lines".to_string());
                    }
                } else {
                    self.message = Some("Move to a diff line to start selection".to_string());
                }
                return Ok(false);
            }
            _ => {}
        }
        match key.code {
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
                    if self.selection_active {
                        self.selection_active = false;
                        self.selection_start = None;
                        self.selection_file_idx = None;
                        self.selection_hunk_idx = None;
                    }
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
            KeyCode::Char(':') => {
                if self.expanded_file.is_some() {
                    self.mode = Mode::GotoLine;
                    self.goto_line_input.clear();
                } else {
                    self.message = Some("Line jump is available in expanded view (x)".to_string());
                }
            }


            // Toggle views
            KeyCode::Char('c') => {
                self.toggle_collapse_current_file();
            }
            KeyCode::Char('h') => {
                if self.expanded_file.is_some() {
                    self.show_old_in_expanded = !self.show_old_in_expanded;
                    let msg = if self.show_old_in_expanded {
                        "Showing deletions".to_string()
                    } else {
                        "Hiding deletions".to_string()
                    };
                    if let Some(file_idx) = self.expanded_file {
                        self.update_display_lines_for_file(file_idx);
                    } else {
                        self.build_display_lines();
                    }
                    self.ensure_cursor_on_navigable();
                    self.adjust_scroll();
                    self.message = Some(msg);
                }
            }
            KeyCode::Char('A') => {
                self.mode = Mode::AnnotationList;
                self.annotation_list_idx = 0;
            }
            KeyCode::Char('r') => {
                if let Some(annotation) = self.get_annotation_for_current_line() {
                    if annotation.resolved_at.is_some() {
                        self.storage.unresolve_annotation(annotation.id)?;
                        self.message = Some("Annotation unresolved".to_string());
                    } else {
                        self.storage.resolve_annotation(annotation.id)?;
                        self.message = Some("Annotation resolved".to_string());
                    }
                    self.load_all_annotations()?;
                    self.build_display_lines();
                } else {
                    self.message = Some("Move to an annotation to resolve".to_string());
                }
            }
            KeyCode::Char('@') => {
                self.spawn_ai_for_current_annotation()?;
            }
            KeyCode::Char('P') => {
                self.show_ai_pane = !self.show_ai_pane;
            }
            KeyCode::Char('s') => {
                self.toggle_stage_current_hunk()?;
            }
            KeyCode::Char('D') => {
                self.discard_current_hunk()?;
            }
            KeyCode::Char('u') => {
                self.toggle_diff_view()?;
            }
            KeyCode::Char('R') => {
                self.reload_diff()?;
                self.message = Some("Diff reloaded".to_string());
            }
            KeyCode::Char('v') => {
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
                    let can_restore = self
                        .pre_expand_diff_generation
                        .map(|gen| gen == self.diff_generation)
                        .unwrap_or(false);
                    if can_restore {
                        if let (Some(lines), Some(ranges)) = (
                            self.pre_expand_display_lines.take(),
                            self.pre_expand_file_line_ranges.take(),
                        ) {
                            self.display_lines = lines;
                            self.file_line_ranges = ranges;
                        } else {
                            self.build_display_lines();
                        }
                    } else {
                        self.build_display_lines();
                    }
                    self.pre_expand_diff_generation = None;

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
                        if !self.diff_loading {
                            self.pre_expand_display_lines = Some(self.display_lines.clone());
                            self.pre_expand_file_line_ranges = Some(self.file_line_ranges.clone());
                            self.pre_expand_diff_generation = Some(self.diff_generation);
                        }
                        let mut target_new: Option<u32> = None;
                        let mut target_old: Option<u32> = None;
                        if let Some(line) = self.current_display_line() {
                            match line {
                                DisplayLine::Diff { line, .. } => {
                                    target_new = line.new_line_no;
                                    target_old = line.old_line_no;
                                }
                                DisplayLine::HunkHeader { line_no, .. } => {
                                    target_new = Some(*line_no);
                                }
                                DisplayLine::HunkContext { line_no, .. } => {
                                    target_new = Some(*line_no);
                                }
                                DisplayLine::HunkEnd { file_idx, hunk_idx } => {
                                    target_new = self
                                        .files
                                        .get(*file_idx)
                                        .and_then(|f| f.hunks.get(*hunk_idx))
                                        .map(|h| h.new_start);
                                }
                                _ => {}
                            }
                        }

                        self.pre_expand_position = Some((self.current_line_idx, self.scroll_offset));
                        self.expanded_file = Some(file_idx);
                        self.build_display_lines();
                        if let Some(line_no) = target_new {
                            if let Some(idx) = self.find_line_in_expanded(line_no) {
                                self.current_line_idx = idx;
                            }
                        } else if let Some(line_no) = target_old {
                            if let Some(idx) = self.find_old_line_in_expanded(line_no) {
                                self.current_line_idx = idx;
                            }
                        }
                        if self.current_line_idx >= self.display_lines.len() {
                            self.current_line_idx = 0;
                        }
                        if self.current_line_idx == 0 {
                            // Skip to first navigable line
                            while self.current_line_idx < self.display_lines.len().saturating_sub(1)
                                && self.is_skippable_line(self.current_line_idx)
                            {
                                self.current_line_idx += 1;
                            }
                            self.scroll_offset = 0;
                        } else {
                            self.center_on_current_line();
                        }
                        self.adjust_scroll();
                        self.message = Some("Expanded file view (]/[: next/prev change, x: collapse)".to_string());
                    }
                }
            }
            KeyCode::Char('y') => {
                if let Some(text) = self.selected_text_for_copy() {
                    self.copy_to_clipboard(&text)?;
                    let lines = text.lines().count().max(1);
                    self.message = Some(format!("Copied {} line{}", lines, if lines == 1 { "" } else { "s" }));
                } else if let Some(DisplayLine::Diff { line, .. }) = self.current_display_line() {
                    self.copy_to_clipboard(&line.content)?;
                    self.message = Some("Copied line".to_string());
                } else {
                    self.message = Some("Move to a diff line or make a selection".to_string());
                }
            }

            // Annotations
            KeyCode::Char('a') => {
                if self.selection_active {
                    if let Some((_, start, end)) = self.selection_range_for_new_side() {
                        self.mode = Mode::AddAnnotation;
                        self.reset_annotation_input();
                        self.annotation_range = Some((start, end));
                        self.annotation_side = Some(Side::New);
                    } else {
                        self.message = Some("Selection has no new/context lines".to_string());
                    }
                } else if let Some(DisplayLine::Diff { line, .. }) = self.current_display_line().cloned() {
                    let side = if line.new_line_no.is_some() {
                        Side::New
                    } else {
                        Side::Old
                    };
                    self.mode = Mode::AddAnnotation;
                    self.reset_annotation_input();
                    self.annotation_range = None;
                    self.annotation_side = Some(side);
                } else {
                    self.message = Some("Move to a diff line to add annotation".to_string());
                }
            }
            KeyCode::Char('e') => {
                if let Some((id, content, a_type)) = self
                    .get_annotation_for_current_line()
                    .map(|a| (a.id, a.content.clone(), a.annotation_type.clone()))
                {
                    self.set_annotation_input(&content);
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

    fn handle_sidebar_input(&mut self, key: KeyEvent) -> Result<bool> {
        let entries = self.sidebar_entries();
        if entries.is_empty() {
            return Ok(false);
        }

        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.sidebar_index + 1 < entries.len() {
                    self.sidebar_index += 1;
                }
                self.focus_file_from_sidebar(&entries);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.sidebar_index > 0 {
                    self.sidebar_index -= 1;
                }
                self.focus_file_from_sidebar(&entries);
            }
            KeyCode::Enter => {
                self.sidebar_focused = false;
            }
            KeyCode::Esc => {
                self.sidebar_focused = false;
            }
            _ => {}
        }
        Ok(false)
    }

    fn focus_file_from_sidebar(&mut self, entries: &[SidebarEntry]) {
        if let Some(entry) = entries.get(self.sidebar_index) {
            if let Some(idx) = self.find_file_header_idx(entry.file_idx) {
                self.current_line_idx = idx;
                self.ensure_cursor_on_navigable();
                self.adjust_scroll();
            }
        }
    }

    fn handle_annotation_input(&mut self, key: KeyEvent) -> Result<bool> {
        // Ctrl+J inserts newline
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('j') {
            self.annotation_input.insert_newline();
            return Ok(false);
        }
        // Ctrl+D / Ctrl+U scroll diff while editing
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
                KeyCode::Char('t') => {
                    self.annotation_type = match self.annotation_type {
                        AnnotationType::Comment => AnnotationType::Todo,
                        AnnotationType::Todo => AnnotationType::Comment,
                    };
                    return Ok(false);
                }
                _ => {}
            }
        }

        if key.modifiers.contains(KeyModifiers::SUPER) {
            match key.code {
                KeyCode::Backspace => {
                    self.annotation_input.delete_line_by_head();
                    return Ok(false);
                }
                KeyCode::Delete => {
                    self.annotation_input.delete_line_by_end();
                    return Ok(false);
                }
                KeyCode::Left => {
                    self.annotation_input.move_cursor(CursorMove::Head);
                    return Ok(false);
                }
                KeyCode::Right => {
                    self.annotation_input.move_cursor(CursorMove::End);
                    return Ok(false);
                }
                _ => {}
            }
        }

        if key.modifiers.contains(KeyModifiers::ALT) {
            match key.code {
                KeyCode::Left => {
                    self.annotation_input.move_cursor(CursorMove::WordBack);
                    return Ok(false);
                }
                KeyCode::Right => {
                    self.annotation_input.move_cursor(CursorMove::WordForward);
                    return Ok(false);
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.reset_annotation_input();
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
            KeyCode::PageUp => {
                self.page_up();
            }
            KeyCode::PageDown => {
                self.page_down();
            }
            _ => {
                self.annotation_input.input(Input::from(key));
            }
        }

        Ok(false)
    }

    fn scroll_by_lines(&mut self, delta: i32) {
        if self.display_lines.is_empty() {
            return;
        }
        let max_offset = self
            .display_lines
            .len()
            .saturating_sub(self.visible_height)
            .max(0);
        let mut offset = self.scroll_offset as i32 + delta;
        if offset < 0 {
            offset = 0;
        }
        if offset as usize > max_offset {
            offset = max_offset as i32;
        }
        self.scroll_offset = offset as usize;
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

    fn handle_goto_line_input(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.goto_line_input.clear();
            }
            KeyCode::Enter => {
                let line_no = self.goto_line_input.trim().parse::<u32>().ok();
                self.mode = Mode::Normal;
                if let Some(line_no) = line_no {
                    if self.expanded_file.is_some() {
                        if let Some(idx) = self.find_line_in_expanded(line_no) {
                            self.current_line_idx = idx;
                            self.ensure_cursor_on_navigable();
                            self.adjust_scroll();
                        } else {
                            self.message = Some(format!("Line {} not found", line_no));
                        }
                    } else {
                        self.message = Some("Line jump is available in expanded view (x)".to_string());
                    }
                } else {
                    self.message = Some("Enter a line number".to_string());
                }
                self.goto_line_input.clear();
            }
            KeyCode::Backspace => {
                self.goto_line_input.pop();
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                self.goto_line_input.push(c);
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
            KeyCode::Char('P') => {
                self.show_ai_pane = !self.show_ai_pane;
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
                    self.set_annotation_input(&entry.content);
                    self.annotation_type = entry.annotation_type.clone();
                    self.mode = Mode::EditAnnotation(entry.id);
                }
            }
            _ => {}
        }

        Ok(false)
    }

    fn handle_ai_event(&mut self, evt: AiEvent) -> Result<()> {
        match evt {
            AiEvent::Output { job_id, chunk } => {
                if let Some(job) = self.ai_jobs.iter_mut().find(|j| j.id == job_id) {
                    match job.target {
                        AiTarget::Claude => {
                            if let Some(text) = extract_claude_text(&chunk) {
                                job.output.push_str(&text);
                            }
                        }
                        AiTarget::Codex => {
                            job.output.push_str(&chunk);
                        }
                    }
                }
            }
            AiEvent::Done { job_id, ok } => {
                let mut resolve_target: Option<(i64, String)> = None;
                if let Some(job) = self.ai_jobs.iter_mut().find(|j| j.id == job_id) {
                    job.status = AiStatus::Done { ok };
                    if ok {
                        resolve_target = Some((job.annotation_id, job.output.clone()));
                    }
                }
                if let Some((annotation_id, output)) = resolve_target {
                    self.apply_ai_resolve(annotation_id, &output)?;
                }
            }
        }
        Ok(())
    }

    fn apply_ai_resolve(&mut self, annotation_id: i64, output: &str) -> Result<()> {
        for line in output.lines() {
            let line = line.trim();
            if line == "RESOLVE_ANNOTATION" {
                self.storage.resolve_annotation(annotation_id)?;
                self.load_all_annotations()?;
                self.build_display_lines();
                return Ok(());
            }
            if let Some(rest) = line.strip_prefix("RESOLVE_ANNOTATION:") {
                let id_str = rest.trim();
                if let Ok(id) = id_str.parse::<i64>() {
                    if id == annotation_id {
                        self.storage.resolve_annotation(id)?;
                        self.load_all_annotations()?;
                        self.build_display_lines();
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
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
            self.jump_to_line(idx);
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

        self.jump_to_line(new_idx);
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

        self.jump_to_line(new_idx);
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
        if self.expanded_file.is_some() {
            self.next_hunk_expanded();
            return;
        }
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
            self.jump_to_line(idx);
            return;
        }

        // Wrap around to beginning
        idx = 0;
        while idx < self.current_line_idx && !self.is_change_line(idx) {
            idx += 1;
        }
        if idx < self.current_line_idx && self.is_change_line(idx) {
            self.jump_to_line(idx);
        }
    }

    /// Jump to previous change chunk (group of consecutive additions/deletions)
    fn prev_change_chunk(&mut self) {
        if self.expanded_file.is_some() {
            self.prev_hunk_expanded();
            return;
        }
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
            self.jump_to_line(idx);
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
            self.jump_to_line(idx);
        }
    }

    fn next_hunk_expanded(&mut self) {
        let total = self.display_lines.len();
        let current_hunk = match self.current_display_line() {
            Some(DisplayLine::Diff { hunk_idx, .. }) => *hunk_idx,
            _ => None,
        };

        let mut idx = self.current_line_idx + 1;
        while idx < total {
            if let Some(DisplayLine::Diff { hunk_idx: Some(h), .. }) = self.display_lines.get(idx) {
                if current_hunk.map_or(true, |ch| ch != *h) {
                    self.jump_to_line(idx);
                    return;
                }
            }
            idx += 1;
        }

        // Wrap to first hunk
        idx = 0;
        while idx < total {
            if let Some(DisplayLine::Diff { hunk_idx: Some(_), .. }) = self.display_lines.get(idx) {
                self.jump_to_line(idx);
                return;
            }
            idx += 1;
        }
    }

    fn prev_hunk_expanded(&mut self) {
        let mut idx = self.current_line_idx;
        let current_hunk = match self.current_display_line() {
            Some(DisplayLine::Diff { hunk_idx, .. }) => *hunk_idx,
            _ => None,
        };

        while idx > 0 {
            idx -= 1;
            if let Some(DisplayLine::Diff { hunk_idx: Some(h), .. }) = self.display_lines.get(idx) {
                if current_hunk.map_or(true, |ch| ch != *h) {
                    // Move to the first line of this hunk
                    let mut start = idx;
                    while start > 0 {
                        if let Some(DisplayLine::Diff { hunk_idx: Some(prev_h), .. }) =
                            self.display_lines.get(start - 1)
                        {
                            if prev_h == h {
                                start -= 1;
                                continue;
                            }
                        }
                        break;
                    }
                    self.jump_to_line(start);
                    return;
                }
            }
        }

        // Wrap to last hunk
        idx = self.display_lines.len().saturating_sub(1);
        while idx > 0 {
            if let Some(DisplayLine::Diff { hunk_idx: Some(h), .. }) = self.display_lines.get(idx) {
                let mut start = idx;
                while start > 0 {
                    if let Some(DisplayLine::Diff { hunk_idx: Some(prev_h), .. }) =
                        self.display_lines.get(start - 1)
                    {
                        if prev_h == h {
                            start -= 1;
                            continue;
                        }
                    }
                    break;
                }
                self.jump_to_line(start);
                return;
            }
            idx -= 1;
        }
    }

    fn current_hunk_ref(&self) -> Option<(usize, usize)> {
        match self.current_display_line() {
            Some(DisplayLine::Diff { file_idx, hunk_idx: Some(hunk_idx), .. }) => {
                Some((*file_idx, *hunk_idx))
            }
            Some(DisplayLine::HunkHeader { file_idx, hunk_idx, .. }) => {
                Some((*file_idx, *hunk_idx))
            }
            Some(DisplayLine::HunkEnd { file_idx, hunk_idx, .. }) => {
                Some((*file_idx, *hunk_idx))
            }
            _ => None,
        }
    }

    fn toggle_stage_current_hunk(&mut self) -> Result<()> {
        let reverse = match self.diff_mode {
            DiffMode::Unstaged => false,
            DiffMode::Staged => true,
            _ => {
                self.message = Some("Staging only works for unstaged/staged diffs".to_string());
                return Ok(());
            }
        };

        let Some((file_idx, hunk_idx)) = self.current_hunk_ref() else {
            self.message = Some("Move to a diff hunk to stage/unstage".to_string());
            return Ok(());
        };

        let Some(file) = self.files.get(file_idx) else {
            self.message = Some("File not found".to_string());
            return Ok(());
        };
        let _file_path = file
            .new_path
            .as_ref()
            .or(file.old_path.as_ref())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        let Some(hunk) = file.hunks.get(hunk_idx) else {
            self.message = Some("Hunk not found".to_string());
            return Ok(());
        };

        let patch = Self::build_hunk_patch(file, hunk);

        match self.apply_patch_to_index(&patch, reverse, file.status) {
            Ok(()) => {
                self.message = Some(if reverse {
                    "Unstaged hunk".to_string()
                } else {
                    "Staged hunk".to_string()
                });
                self.invalidate_pre_expand_cache();
                self.apply_stage_local(file_idx, hunk_idx)?;
            }
            Err(err) => {
                self.message = Some(format!("Stage/unstage failed: {}", err));
            }
        }

        Ok(())
    }

    fn discard_current_hunk(&mut self) -> Result<()> {
        if !matches!(self.diff_mode, DiffMode::Unstaged) {
            self.message = Some("Discard only works for unstaged changes".to_string());
            return Ok(());
        }

        let Some((file_idx, hunk_idx)) = self.current_hunk_ref() else {
            self.message = Some("Move to a diff hunk to discard".to_string());
            return Ok(());
        };

        let Some(file) = self.files.get(file_idx) else {
            self.message = Some("File not found".to_string());
            return Ok(());
        };
        let Some(hunk) = file.hunks.get(hunk_idx) else {
            self.message = Some("Hunk not found".to_string());
            return Ok(());
        };

        let patch = Self::build_hunk_patch(file, hunk);

        match self.apply_patch_to_worktree(&patch, true) {
            Ok(()) => {
                self.message = Some("Discarded hunk".to_string());
                self.invalidate_pre_expand_cache();
                self.apply_discard_local(file_idx, hunk_idx)?;
            }
            Err(err) => {
                self.message = Some(format!("Discard failed: {}", err));
            }
        }

        Ok(())
    }

    fn apply_stage_local(&mut self, file_idx: usize, hunk_idx: usize) -> Result<()> {
        if file_idx >= self.files.len() {
            return Ok(());
        }
        let (old_path, new_path, status, moved_hunk) = match self.files.get(file_idx) {
            Some(file) => match file.hunks.get(hunk_idx) {
                Some(hunk) => (
                    file.old_path.clone(),
                    file.new_path.clone(),
                    file.status,
                    hunk.clone(),
                ),
                None => return Ok(()),
            },
            None => return Ok(()),
        };

        if let Some(file) = self.files.get_mut(file_idx) {
            if hunk_idx < file.hunks.len() {
                file.hunks.remove(hunk_idx);
            }
        }
        let empty = self.files.get(file_idx).map(|f| f.hunks.is_empty()).unwrap_or(true);
        if empty {
            self.files.remove(file_idx);
            self.diff_file_index
                .retain(|_, idx| *idx != file_idx);
            for idx in self.diff_file_index.values_mut() {
                if *idx > file_idx {
                    *idx -= 1;
                }
            }
            self.file_line_ranges.remove(file_idx);
            self.build_display_lines();
            self.ensure_cursor_on_navigable();
            self.adjust_scroll();
            return Ok(());
        }

        self.update_display_lines_for_file(file_idx);
        self.ensure_cursor_on_navigable();
        self.adjust_scroll();
        self.apply_stage_other_cache(old_path, new_path, status, moved_hunk);
        Ok(())
    }

    fn apply_discard_local(&mut self, file_idx: usize, hunk_idx: usize) -> Result<()> {
        if file_idx >= self.files.len() {
            return Ok(());
        }
        if let Some(file) = self.files.get_mut(file_idx) {
            if hunk_idx < file.hunks.len() {
                file.hunks.remove(hunk_idx);
            }
        }

        let empty = self.files.get(file_idx).map(|f| f.hunks.is_empty()).unwrap_or(true);
        if empty {
            self.files.remove(file_idx);
            self.diff_file_index
                .retain(|_, idx| *idx != file_idx);
            for idx in self.diff_file_index.values_mut() {
                if *idx > file_idx {
                    *idx -= 1;
                }
            }
            self.file_line_ranges.remove(file_idx);
            self.build_display_lines();
            self.ensure_cursor_on_navigable();
            self.adjust_scroll();
        } else {
            self.update_display_lines_for_file(file_idx);
            self.ensure_cursor_on_navigable();
            self.adjust_scroll();
        }

        if matches!(self.diff_mode, DiffMode::Unstaged) {
            self.cached_unstaged = Some(self.files.clone());
        }
        Ok(())
    }

    fn apply_stage_other_cache(
        &mut self,
        old_path: Option<PathBuf>,
        new_path: Option<PathBuf>,
        status: FileStatus,
        hunk: DiffHunk,
    ) {
        let target = match self.diff_mode {
            DiffMode::Unstaged => &mut self.cached_staged,
            DiffMode::Staged => &mut self.cached_unstaged,
            _ => return,
        };

        let Some(files) = target.as_mut() else {
            return;
        };

        let key = new_path
            .as_ref()
            .or(old_path.as_ref())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "<unknown>".to_string());

        if let Some(file) = files.iter_mut().find(|f| {
            let fkey = f
                .new_path
                .as_ref()
                .or(f.old_path.as_ref())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            fkey == key
        }) {
            let insert_at = file
                .hunks
                .iter()
                .position(|h| h.new_start > hunk.new_start)
                .unwrap_or(file.hunks.len());
            file.hunks.insert(insert_at, hunk);
        } else {
            files.push(DiffFile {
                old_path,
                new_path,
                status,
                hunks: vec![hunk],
            });
        }
    }

    fn spawn_ai_for_current_annotation(&mut self) -> Result<()> {
        let annotation = if let Some(DisplayLine::Annotation { annotation, .. }) =
            self.current_display_line()
        {
            annotation.clone()
        } else if matches!(self.mode, Mode::AnnotationList) {
            let entries = self.annotation_list_entries();
            let entry = entries
                .get(self.annotation_list_idx)
                .ok_or_else(|| anyhow!("Annotation not found"))?;
            let entry_id = entry.id;
            self.all_annotations
                .iter()
                .find(|a| a.id == entry_id)
                .cloned()
                .ok_or_else(|| anyhow!("Annotation not found"))?
        } else if let Some(annotation) = self.get_annotation_for_current_line().cloned() {
            annotation
        } else {
            self.message = Some("Move to an annotation to send to AI".to_string());
            return Ok(());
        };

        let prompt = self.build_ai_prompt(&annotation);
        let job_id = self.ai_next_id;
        self.ai_next_id += 1;

        let job = AiJob {
            id: job_id,
            annotation_id: annotation.id,
            annotation_type: annotation.annotation_type.clone(),
            file_path: annotation.file_path.clone(),
            line: annotation.start_line,
            status: AiStatus::Running,
            output: String::new(),
            target: self.config.ai_target,
        };
        self.ai_jobs.push(job);

        let ai_target = self.config.ai_target;
        let repo_path = self.repo_path.clone();
        let tx = self.ai_tx.clone();
        std::thread::spawn(move || {
            let result = run_ai_process(&ai_target, &prompt, &repo_path, |chunk| {
                let _ = tx.send(AiEvent::Output {
                    job_id,
                    chunk: chunk.to_string(),
                });
            });
            let ok = result.is_ok();
            let _ = tx.send(AiEvent::Done { job_id, ok });
        });

        Ok(())
    }

    fn build_ai_prompt(&self, annotation: &Annotation) -> String {
        let mut prompt = String::new();
        prompt.push_str("You are a coding agent. Use the instruction below to act.\n\n");
        prompt.push_str("Instruction:\n");
        prompt.push_str(&format!(
            "- Annotation ID: {}\n- File: {}\n- Line: {}\n- Side: {}\n- Type: {}\n",
            annotation.id,
            annotation.file_path,
            annotation.start_line,
            annotation.side.as_str(),
            annotation.annotation_type.as_str()
        ));
        prompt.push_str("\nAnnotation:\n");
        prompt.push_str(&annotation.content);
        prompt.push_str("\n\nContext (before):\n");
        prompt.push_str(&annotation.context_before);
        prompt.push_str("\n\nContext (after):\n");
        prompt.push_str(&annotation.context_after);
        prompt.push_str("\n\nIf you complete the task, output a single line:\n");
        prompt.push_str(&format!("RESOLVE_ANNOTATION: {}\n", annotation.id));
        prompt
    }

    fn toggle_diff_view(&mut self) -> Result<()> {
        let target = match self.diff_mode {
            DiffMode::Unstaged => DiffMode::Staged,
            DiffMode::Staged => DiffMode::Unstaged,
            _ => {
                self.message = Some("Only unstaged/staged views can be toggled".to_string());
                return Ok(());
            }
        };

        self.switch_diff_mode(target)
    }

    fn switch_diff_mode(&mut self, target: DiffMode) -> Result<()> {
        self.save_collapsed_state();
        self.diff_mode = target;
        self.load_collapsed_state();
        self.start_diff_stream(None)?;

        Ok(())
    }

    fn build_hunk_patch(file: &DiffFile, hunk: &DiffHunk) -> String {
        let old_path = file
            .old_path
            .as_ref()
            .or(file.new_path.as_ref())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let new_path = file
            .new_path
            .as_ref()
            .or(file.old_path.as_ref())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| old_path.clone());

        let mut patch = String::new();
        patch.push_str(&format!("diff --git a/{} b/{}\n", old_path, new_path));

        match file.status {
            FileStatus::Added => {
                patch.push_str("new file mode 100644\n");
                patch.push_str("--- /dev/null\n");
                patch.push_str(&format!("+++ b/{}\n", new_path));
            }
            FileStatus::Deleted => {
                patch.push_str(&format!("--- a/{}\n", old_path));
                patch.push_str("+++ /dev/null\n");
            }
            FileStatus::Modified | FileStatus::Renamed => {
                patch.push_str(&format!("--- a/{}\n", old_path));
                patch.push_str(&format!("+++ b/{}\n", new_path));
            }
        }

        patch.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
        ));

        for line in &hunk.lines {
            let prefix = match line.kind {
                LineKind::Context => ' ',
                LineKind::Addition => '+',
                LineKind::Deletion => '-',
            };
            patch.push(prefix);
            patch.push_str(&line.content);
            patch.push('\n');
        }

        patch
    }

    fn apply_patch_to_index(
        &self,
        patch: &str,
        reverse: bool,
        _status: FileStatus,
    ) -> Result<(), String> {
        let mut cmd = Command::new("git");
        cmd.arg("apply").arg("--cached");
        if reverse {
            cmd.arg("-R");
        } else if patch.contains("--- /dev/null\n") && !patch.contains("new file mode") {
            // Allow staging hunks for new files not yet in the index when
            // the patch doesn't already declare a new file.
            cmd.arg("--intent-to-add");
        }
        cmd.current_dir(&self.repo_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(patch.as_bytes()).map_err(|e| e.to_string())?;
        }
        let output = child.wait_with_output().map_err(|e| e.to_string())?;

        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if stderr.is_empty() { "git apply failed".to_string() } else { stderr })
        }
    }

    fn apply_patch_to_worktree(&self, patch: &str, reverse: bool) -> Result<(), String> {
        let mut cmd = Command::new("git");
        cmd.arg("apply");
        if reverse {
            cmd.arg("-R");
        }
        cmd.current_dir(&self.repo_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(patch.as_bytes()).map_err(|e| e.to_string())?;
        }
        let output = child.wait_with_output().map_err(|e| e.to_string())?;

        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if stderr.is_empty() { "git apply failed".to_string() } else { stderr })
        }
    }

    fn reload_diff(&mut self) -> Result<()> {
        let target = self.current_target_position();
        self.start_diff_stream(target)?;
        Ok(())
    }

    fn invalidate_pre_expand_cache(&mut self) {
        self.pre_expand_display_lines = None;
        self.pre_expand_file_line_ranges = None;
        self.pre_expand_diff_generation = None;
    }

    fn handle_fs_pending(&mut self) -> Result<()> {
        if !self.fs_pending {
            return Ok(());
        }
        let Some(last) = self.fs_last_event else {
            return Ok(());
        };
        if last.elapsed() < Duration::from_millis(300) {
            return Ok(());
        }
        if self.diff_loading || !matches!(self.mode, Mode::Normal) {
            return Ok(());
        }
        self.fs_pending = false;
        self.fs_last_event = None;
        self.reload_diff()?;
        self.message = Some("Diff reloaded".to_string());
        Ok(())
    }

    fn clear_syntax_cache(&mut self) {
        self.syntax_cache_old.clear();
        self.syntax_cache_new.clear();
        self.expanded_highlight_files.clear();
        self.full_highlight_pending.clear();
        self.full_highlight_queue.clear();
        self.full_highlight_inflight = 0;
    }

    fn start_diff_stream(&mut self, target: Option<(String, u32)>) -> Result<()> {
        let generation = self.diff_generation.wrapping_add(1);
        self.diff_generation = generation;
        self.diff_loading = true;
        self.invalidate_pre_expand_cache();
        // Keep current position while streaming the new diff.
        self.clear_syntax_cache();
        self.load_all_annotations()?;
        self.selection_active = false;
        self.selection_start = None;
        self.selection_file_idx = None;
        self.selection_hunk_idx = None;
        self.annotation_range = None;

        let diff_engine = self.diff_engine.clone();
        let diff_mode = self.diff_mode.clone();
        let diff_paths = self.diff_paths.clone();
        let tx = self.diff_tx.clone();

        thread::spawn(move || {
            let mut on_file = |file: DiffFile| -> Result<()> {
                let _ = tx.send(DiffStreamEvent::File { generation, file });
                Ok(())
            };
            let result = diff_engine.diff_stream(&diff_mode, &diff_paths, &mut on_file);
            let error = result.err().map(|e| e.to_string());
            let _ = tx.send(DiffStreamEvent::Done { generation, error });
        });

        self.message = Some("Loading diff...".to_string());

        self.diff_target = target;
        self.diff_pending_reset = true;

        Ok(())
    }

    fn handle_diff_event(&mut self, evt: DiffStreamEvent) -> Result<()> {
        match evt {
            DiffStreamEvent::File { generation, file } => {
                if generation != self.diff_generation {
                    return Ok(());
                }
                if self.diff_pending_reset {
                    self.files.clear();
                    self.diff_file_index.clear();
                    self.display_lines.clear();
                    self.file_line_ranges.clear();
                    self.pending_stream_files.clear();
                    self.diff_pending_reset = false;
                }
                self.pending_stream_files.push(file);
                let should_flush =
                    self.files.is_empty() || self.pending_stream_files.len() >= STREAM_BATCH_FILES;
                if should_flush {
                    self.flush_streamed_files();
                }
            }
            DiffStreamEvent::Done { generation, error } => {
                if generation != self.diff_generation {
                    return Ok(());
                }
                if self.sidebar_open {
                    let total = self.files.len();
                    if total == 0 {
                        self.sidebar_index = 0;
                        self.sidebar_scroll = 0;
                    } else if self.sidebar_index >= total {
                        self.sidebar_index = total.saturating_sub(1);
                    }
                }
                self.flush_streamed_files();
                self.diff_loading = false;
                if let Some(err) = error {
                    self.message = Some(format!("Diff load failed: {}", err));
                } else if self.files.is_empty() {
                    let msg = match self.diff_mode {
                        DiffMode::Unstaged => "No unstaged changes",
                        DiffMode::Staged => "No staged changes",
                        _ => "No changes",
                    };
                    self.message = Some(msg.to_string());
                } else {
                    self.message = None;
                }
                if !self.files.is_empty() {
                    if self.display_lines.is_empty() {
                        self.build_display_lines();
                    }
                }
                if let Some(search) = &mut self.search {
                    if !search.query.is_empty() {
                        self.execute_search();
                    }
                }
                match self.diff_mode {
                    DiffMode::Unstaged => self.cached_unstaged = Some(self.files.clone()),
                    DiffMode::Staged => self.cached_staged = Some(self.files.clone()),
                    _ => {}
                }
                if self.diff_pending_reset {
                    self.files.clear();
                    self.diff_file_index.clear();
                    self.display_lines.clear();
                    self.file_line_ranges.clear();
                    self.diff_pending_reset = false;
                }
                if let Some((file_path, line_no)) = self.diff_target.as_ref() {
                    if let Some(idx) = self.find_best_line_match(file_path, *line_no) {
                        self.diff_target = None;
                        self.current_line_idx = idx;
                        self.ensure_cursor_on_navigable();
                        self.adjust_scroll();
                    }
                }
            }
        }
        Ok(())
    }

    fn flush_streamed_files(&mut self) {
        if self.pending_stream_files.is_empty() {
            return;
        }
        let mut updated_indices = Vec::new();
        let files = mem::take(&mut self.pending_stream_files);
        for file in files {
            let (file_key, _, _) = Self::file_highlight_keys(&file);
            if let Some(idx) = self.diff_file_index.get(&file_key).copied() {
                if let Some(slot) = self.files.get_mut(idx) {
                    *slot = file;
                }
                updated_indices.push(idx);
            } else {
                let idx = self.files.len();
                self.files.push(file);
                self.diff_file_index.insert(file_key, idx);
                updated_indices.push(idx);
            }
        }

        for idx in updated_indices {
            self.update_display_lines_for_file(idx);
        }

        if !self.diff_loading {
            if let Some(search) = &mut self.search {
                if !search.query.is_empty() {
                    self.execute_search();
                }
            }
        }

        if let Some((file_path, line_no)) = self.diff_target.as_ref() {
            if let Some(idx) = self.find_best_line_match(file_path, *line_no) {
                self.diff_target = None;
                self.current_line_idx = idx;
                self.ensure_cursor_on_navigable();
                self.adjust_scroll();
            }
        } else if self.current_line_idx == 0 {
            self.ensure_cursor_on_navigable();
            self.adjust_scroll();
        } else if self.current_line_idx >= self.display_lines.len().saturating_sub(1) {
            self.current_line_idx = self.display_lines.len().saturating_sub(1);
            self.ensure_cursor_on_navigable();
            self.adjust_scroll();
        }
    }

    fn handle_full_highlight_event(&mut self, evt: HighlightEvent) -> Result<()> {
        match evt {
            HighlightEvent::FullFile {
                generation,
                file_key,
                map,
            } => {
                if generation != self.diff_generation {
                    return Ok(());
                }
                self.syntax_cache_new.insert(file_key.clone(), map);
                self.expanded_highlight_files.insert(file_key.clone());
                self.full_highlight_pending.remove(&file_key);
                if self.full_highlight_inflight > 0 {
                    self.full_highlight_inflight -= 1;
                }
                self.kick_full_highlight_jobs();
                if let Some(idx) = self.diff_file_index.get(&file_key).copied() {
                    self.update_display_lines_for_file(idx);
                }
            }
        }
        Ok(())
    }

    fn update_display_lines_for_file(&mut self, file_idx: usize) {
        if file_idx >= self.files.len() {
            return;
        }
        if self.file_line_ranges.len() < self.files.len() {
            self.file_line_ranges
                .resize(self.files.len(), None);
        }

        if let Some(expanded_idx) = self.expanded_file {
            if expanded_idx != file_idx {
                return;
            }
            let file = self.files[file_idx].clone();
            let file_path = file
                .new_path
                .as_ref()
                .or(file.old_path.as_ref())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            let lines = self.build_file_lines(file_idx, &file, &file_path, false);
            self.display_lines = lines;
            self.file_line_ranges = vec![None; self.files.len()];
            let len = self.display_lines.len();
            if len > 0 {
                self.file_line_ranges[file_idx] = Some((0, len));
            }
            return;
        }

        let file = self.files[file_idx].clone();
        let file_path = file
            .new_path
            .as_ref()
            .or(file.old_path.as_ref())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        let include_spacer = self.file_line_ranges.iter().take(file_idx).any(|r| r.is_some());
        let new_lines = self.build_file_lines(file_idx, &file, &file_path, include_spacer);
        let new_len = new_lines.len();

        if let Some((start, len)) = self.file_line_ranges.get(file_idx).copied().flatten() {
            let end = start + len;
            self.display_lines.splice(start..end, new_lines);
            let delta = new_len as isize - len as isize;
            self.file_line_ranges[file_idx] = Some((start, new_len));
            if delta != 0 {
                for idx in (file_idx + 1)..self.file_line_ranges.len() {
                    if let Some((s, l)) = self.file_line_ranges[idx] {
                        let new_start = (s as isize + delta).max(0) as usize;
                        self.file_line_ranges[idx] = Some((new_start, l));
                    }
                }
            }
        } else {
            let mut insert_at = 0usize;
            for idx in 0..file_idx {
                if let Some((s, l)) = self.file_line_ranges[idx] {
                    insert_at = s + l;
                }
            }
            self.display_lines.splice(insert_at..insert_at, new_lines);
            self.file_line_ranges[file_idx] = Some((insert_at, new_len));
            for idx in (file_idx + 1)..self.file_line_ranges.len() {
                if let Some((s, l)) = self.file_line_ranges[idx] {
                    self.file_line_ranges[idx] = Some((s + new_len, l));
                }
            }
        }

        if self.current_line_idx >= self.display_lines.len() && !self.display_lines.is_empty() {
            self.current_line_idx = self.display_lines.len().saturating_sub(1);
        }
        if self.current_line_idx == 0 {
            self.ensure_cursor_on_navigable();
        }
        self.adjust_scroll();
    }

    fn save_collapsed_state(&mut self) {
        match self.diff_mode {
            DiffMode::Unstaged => self.collapsed_files_unstaged = self.collapsed_files.clone(),
            DiffMode::Staged => self.collapsed_files_staged = self.collapsed_files.clone(),
            _ => self.collapsed_files_other = self.collapsed_files.clone(),
        }
    }

    fn load_collapsed_state(&mut self) {
        self.collapsed_files = match self.diff_mode {
            DiffMode::Unstaged => self.collapsed_files_unstaged.clone(),
            DiffMode::Staged => self.collapsed_files_staged.clone(),
            _ => self.collapsed_files_other.clone(),
        };
    }

    fn current_target_position(&self) -> Option<(String, u32)> {
        match self.current_display_line() {
            Some(DisplayLine::Diff { line, file_path, .. }) => {
                let line_no = line.new_line_no.or(line.old_line_no)?;
                Some((file_path.clone(), line_no))
            }
            Some(DisplayLine::Annotation { annotation, .. }) => {
                Some((annotation.file_path.clone(), annotation.start_line))
            }
            _ => self.current_file_info().map(|(path, _)| (path, 1)),
        }
    }

    fn sidebar_entries(&self) -> Vec<SidebarEntry> {
        self.files
            .iter()
            .enumerate()
            .map(|(idx, file)| {
                let path = file
                    .new_path
                    .as_ref()
                    .or(file.old_path.as_ref())
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|| "<unknown>".to_string());
                SidebarEntry {
                    file_idx: idx,
                    path,
                    status: file.status,
                }
            })
            .collect()
    }

    fn find_file_header_idx(&self, file_idx: usize) -> Option<usize> {
        for (idx, line) in self.display_lines.iter().enumerate() {
            if let DisplayLine::FileHeader { file_idx: header_idx, .. } = line {
                if *header_idx == file_idx {
                    return Some(idx);
                }
            }
        }
        None
    }

    fn find_best_line_match(&self, file_path: &str, line_no: u32) -> Option<usize> {
        let mut best: Option<(usize, u32)> = None;
        for (idx, line) in self.display_lines.iter().enumerate() {
            if let DisplayLine::Diff { line, file_path: fp, .. } = line {
                if fp != file_path {
                    continue;
                }
                if let Some(ln) = line.new_line_no.or(line.old_line_no) {
                    let dist = ln.abs_diff(line_no);
                    match best {
                        Some((_, best_dist)) if dist >= best_dist => {}
                        _ => best = Some((idx, dist)),
                    }
                }
            }
        }
        best.map(|(idx, _)| idx)
    }

    fn toggle_collapse_current_file(&mut self) {
        let current_header_idx = self.find_current_file_header_idx();
        let Some((file_path, file_idx)) = self.current_file_info() else {
            return;
        };

        let was_collapsed = self.collapsed_files.contains(&file_path);
        if was_collapsed {
            self.collapsed_files.remove(&file_path);
            self.message = Some("Expanded file".to_string());
        } else {
            self.collapsed_files.insert(file_path);
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
        self.selection_active = false;
        self.selection_start = None;
        self.selection_file_idx = None;
        self.selection_hunk_idx = None;
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
        let mut visible: HashMap<i64, (usize, bool)> = HashMap::new();
        for (idx, line) in self.display_lines.iter().enumerate() {
            if let DisplayLine::Annotation { annotation, orphaned, .. } = line {
                visible.insert(annotation.id, (idx, *orphaned));
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
                display_idx: visible.get(&a.id).map(|(idx, _)| *idx),
                orphaned: visible.get(&a.id).map(|(_, o)| *o).unwrap_or(true),
                resolved: a.resolved_at.is_some(),
            })
            .collect();

        entries.sort_by(|a, b| a.file_path.cmp(&b.file_path).then(a.line.cmp(&b.line)));
        entries
    }

    fn annotation_text(&self) -> String {
        self.annotation_input.lines().join("\n")
    }

    fn reset_annotation_input(&mut self) {
        self.annotation_input = TextArea::default();
    }

    fn set_annotation_input(&mut self, content: &str) {
        self.annotation_input = TextArea::from(content.lines());
        self.annotation_input.move_cursor(CursorMove::Bottom);
        self.annotation_input.move_cursor(CursorMove::End);
    }

    fn render_input_with_cursor(&self) -> String {
        let (row, col) = self.annotation_input.cursor();
        let lines = self.annotation_input.lines();
        let mut out = String::new();

        for (idx, line) in lines.iter().enumerate() {
            if idx == row {
                let mut chars: Vec<char> = line.chars().collect();
                let cursor = col.min(chars.len());
                chars.insert(cursor, '|');
                out.push_str(&chars.into_iter().collect::<String>());
            } else {
                out.push_str(line);
            }
            if idx + 1 < lines.len() {
                out.push('\n');
            }
        }
        out
    }

    /// Adjust scroll to keep cursor visible with vim-like margins
    fn adjust_scroll(&mut self) {
        let scroll_margin = 5; // Keep 5 lines of padding

        if self.display_lines.is_empty() || self.visible_height == 0 {
            return;
        }

        if self.current_line_idx < self.scroll_offset {
            self.scroll_offset = self.current_line_idx;
        }

        let current_height = self.display_line_height(self.current_line_idx);
        let bottom_limit = self
            .visible_height
            .saturating_sub(scroll_margin + current_height);
        let limit = self
            .visible_height
            .saturating_add(scroll_margin + current_height);
        let row_pos = self.rows_between_limited(self.scroll_offset, self.current_line_idx, limit);

        if row_pos < scroll_margin {
            self.scroll_offset = self.scroll_offset_for_top(scroll_margin);
        } else if row_pos > bottom_limit {
            self.scroll_offset = self.scroll_offset_for_bottom(bottom_limit);
        }

        self.clamp_scroll();
        self.sync_sidebar_to_current_file();
    }

    fn jump_to_line(&mut self, idx: usize) {
        if self.display_lines.is_empty() {
            return;
        }
        self.current_line_idx = idx.min(self.display_lines.len().saturating_sub(1));
        self.ensure_cursor_on_navigable();
        self.adjust_scroll();
    }

    fn rows_between_limited(&self, start: usize, end: usize, limit: usize) -> usize {
        if start >= end {
            return 0;
        }
        let mut rows = 0usize;
        for idx in start..end {
            rows = rows.saturating_add(self.display_line_height(idx));
            if rows > limit {
                break;
            }
        }
        rows
    }

    fn scroll_offset_for_top(&self, target_top: usize) -> usize {
        let mut cum = 0usize;
        let mut idx = self.current_line_idx;
        while idx > 0 && cum < target_top {
            idx -= 1;
            cum = cum.saturating_add(self.display_line_height(idx));
        }
        idx
    }

    fn scroll_offset_for_bottom(&self, target_bottom: usize) -> usize {
        let mut cum = 0usize;
        let mut idx = self.current_line_idx;
        while idx > 0 {
            let prev = idx - 1;
            let h = self.display_line_height(prev);
            if cum.saturating_add(h) > target_bottom {
                break;
            }
            idx = prev;
            cum = cum.saturating_add(h);
        }
        idx
    }

    fn display_line_height(&self, idx: usize) -> usize {
        let Some(line) = self.display_lines.get(idx) else {
            return 1;
        };
        let content_width = self.content_width.max(1);
        match line {
            DisplayLine::Spacer
            | DisplayLine::FileHeader { .. }
            | DisplayLine::HunkHeader { .. }
            | DisplayLine::HunkEnd { .. } => 1,
            DisplayLine::HunkContext { .. } => {
                if content_width < 4 { 1 } else { 3 }
            }
            DisplayLine::Diff { line, .. } => {
                let prefix_width = if self.side_by_side { 7 } else { 8 };
                let content_width = content_width.saturating_sub(prefix_width).max(1);
                let width = UnicodeWidthStr::width(line.content.as_str());
                if width == 0 {
                    1
                } else {
                    (width + content_width - 1) / content_width
                }
            }
            DisplayLine::Annotation { annotation, orphaned, .. } => {
                let resolved = annotation.resolved_at.is_some();
                let prefix = match annotation.annotation_type {
                    AnnotationType::Comment => {
                        if resolved {
                            if self.side_by_side { "   ✓ RESOLVED " } else { "    ✓ RESOLVED " }
                        } else if *orphaned {
                            if self.side_by_side { "   ⚠ ORPHANED " } else { "    ⚠ ORPHANED " }
                        } else if self.side_by_side {
                            "   💬 "
                        } else {
                            "    💬 "
                        }
                    }
                    AnnotationType::Todo => {
                        if resolved {
                            if self.side_by_side { "   ✓ RESOLVED " } else { "    ✓ RESOLVED " }
                        } else if *orphaned {
                            if self.side_by_side { "   ⚠ ORPHANED " } else { "    ⚠ ORPHANED " }
                        } else if self.side_by_side {
                            "   📌 "
                        } else {
                            "    📌 "
                        }
                    }
                };
                let prefix_width = UnicodeWidthStr::width(prefix);
                let content_width = content_width.saturating_sub(prefix_width).max(1);
                let content = if annotation.content.is_empty() {
                    ""
                } else {
                    annotation.content.as_str()
                };
                let mut total = 0usize;
                for line in content.split('\n') {
                    let width = UnicodeWidthStr::width(line);
                    let lines = if width == 0 {
                        1
                    } else {
                        (width + content_width - 1) / content_width
                    };
                    total = total.saturating_add(lines);
                }
                if total == 0 { 1 } else { total }
            }
        }
    }

    fn clamp_scroll(&mut self) {
        if self.display_lines.is_empty() || self.visible_height == 0 {
            return;
        }
        let max_scroll = self.max_scroll_offset();
        if self.scroll_offset > max_scroll {
            self.scroll_offset = max_scroll;
        }
    }

    fn sync_sidebar_to_current_file(&mut self) {
        if !self.sidebar_open || self.sidebar_focused {
            return;
        }
        if let Some((_, file_idx)) = self.current_file_info() {
            self.sidebar_index = file_idx.min(self.files.len().saturating_sub(1));
        }
    }

    fn max_scroll_offset(&self) -> usize {
        if self.display_lines.is_empty() || self.visible_height == 0 {
            return 0;
        }
        let mut cum = 0usize;
        let mut idx = self.display_lines.len();
        while idx > 0 && cum < self.visible_height {
            idx -= 1;
            cum = cum.saturating_add(self.display_line_height(idx));
        }
        if cum < self.visible_height {
            0
        } else {
            idx
        }
    }

    fn center_on_current_line(&mut self) {
        if self.visible_height == 0 {
            return;
        }
        let half = self.visible_height / 2;
        self.scroll_offset = self.current_line_idx.saturating_sub(half);
        self.clamp_scroll();
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
    diff_mode: DiffMode,
    diff_paths: Vec<String>,
) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    execute!(stdout, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(
        storage,
        diff_engine,
        repo_path,
        repo_id,
        files,
        config,
        diff_mode,
        diff_paths,
    )?;
    app.start_diff_stream(None)?;
    let _watcher = match start_fs_watcher(
        app.repo_path.clone(),
        app.fs_tx.clone(),
        app.config.watch_ignore_paths.clone(),
    ) {
        Ok(watcher) => Some(watcher),
        Err(err) => {
            app.message = Some(format!("Watcher disabled: {}", err));
            None
        }
    };

    // Skip to first navigable line
    while app.current_line_idx < app.display_lines.len().saturating_sub(1)
        && app.is_skippable_line(app.current_line_idx)
    {
        app.current_line_idx += 1;
    }

    let result = run_app(&mut terminal, &mut app);

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), DisableMouseCapture)?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    result
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        while let Ok(evt) = app.ai_rx.try_recv() {
            app.handle_ai_event(evt)?;
        }
        while let Ok(evt) = app.full_highlight_rx.try_recv() {
            app.handle_full_highlight_event(evt)?;
        }
        while let Ok(evt) = app.diff_rx.try_recv() {
            app.handle_diff_event(evt)?;
        }
        while let Ok(_) = app.fs_rx.try_recv() {
            app.fs_pending = true;
            app.fs_last_event = Some(Instant::now());
        }
        app.handle_fs_pending()?;

        if event::poll(std::time::Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    if app.handle_input(key)? {
                        return Ok(());
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        if app.show_help {
                            app.help_scroll_up();
                        } else {
                            app.scroll_by_lines(-3);
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if app.show_help {
                            app.help_scroll_down();
                        } else {
                            app.scroll_by_lines(3);
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
}

fn start_fs_watcher(
    repo_path: PathBuf,
    tx: Sender<FsEvent>,
    ignore_paths: Vec<String>,
) -> Result<RecommendedWatcher> {
    let ignore_paths = ignore_paths;
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<FsEventRaw>| {
        if let Ok(event) = res {
            if should_ignore_fs_event(&event, &ignore_paths) {
                return;
            }
            let _ = tx.send(FsEvent::Changed);
        }
    })
    .context("Failed to create filesystem watcher")?;
    watcher
        .watch(&repo_path, RecursiveMode::Recursive)
        .context("Failed to watch repo path")?;
    Ok(watcher)
}

fn should_ignore_fs_event(event: &FsEventRaw, ignore_paths: &[String]) -> bool {
    match event.kind {
        EventKind::Access(_) => return true,
        EventKind::Modify(ModifyKind::Metadata(_)) => return true,
        _ => {}
    }
    if event
        .paths
        .iter()
        .all(|p| is_ignored_fs_path(p, ignore_paths))
    {
        return true;
    }
    false
}

fn is_ignored_fs_path(path: &Path, ignore_paths: &[String]) -> bool {
    path.components().any(|component| {
        let part = component.as_os_str().to_string_lossy();
        ignore_paths.iter().any(|ignore| ignore == &part)
    })
}

fn ui(f: &mut Frame, app: &mut App) {
    let theme = Theme::default();

    // Expand input area when in annotation mode
    let input_height = match app.mode {
        Mode::Normal => 1,
        _ => {
            let inner_width = f.area().width.saturating_sub(2) as usize;
            let input_with_cursor = app.render_input_with_cursor();
            let line_count = wrapped_line_count(&input_with_cursor, inner_width.max(1));
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

    // Diff content (optional sidebar)
    let diff_area = if app.sidebar_open {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(32), Constraint::Min(0)])
            .split(chunks[1]);
        render_sidebar(f, app, cols[0], theme);
        let mut diff_border = Style::default().fg(theme.border);
        if app.sidebar_focused {
            diff_border = diff_border.add_modifier(Modifier::DIM);
        } else {
            diff_border = diff_border.add_modifier(Modifier::BOLD);
        }
        let diff_block = Block::default()
            .borders(Borders::LEFT)
            .border_style(diff_border);
        let inner = diff_block.inner(cols[1]);
        f.render_widget(diff_block, cols[1]);
        inner
    } else {
        chunks[1]
    };

    f.render_widget(Clear, diff_area);
    if app.side_by_side {
        render_diff_side_by_side(f, app, diff_area, theme);
    } else {
        render_diff_unified(f, app, diff_area, theme);
    }

    // Status bar / input
    render_status(f, app, chunks[2], theme);

    // Help overlay
    if app.show_help {
        let help_area = centered_rect(60, 80, f.area());
        app.help_visible_height = help_area.height.saturating_sub(2) as usize;
        render_help(f, app, help_area, theme);
    }

    if matches!(app.mode, Mode::AnnotationList) {
        render_annotation_list(f, app, theme);
    }

    if app.show_ai_pane {
        render_ai_pane(f, app, theme);
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
    let is_collapsed = app.collapsed_files.contains(&file_path);

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


fn render_diff_unified(f: &mut Frame, app: &mut App, area: Rect, theme: Theme) {
    let visible_height = area.height as usize;
    app.content_width = area.width as usize;
    let scroll_offset = app.scroll_offset;

    let items: Vec<ListItem> = app
        .display_lines
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_height)
        .map(|(idx, display_line)| {
            let is_current = idx == app.current_line_idx;

            let selection_range = app.selection_range();
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

                    let collapse_indicator = if app.collapsed_files.contains(path) { " ▶" } else { "" };

                    ListItem::new(Line::from(vec![
                        Span::styled(format!(" Δ {}{}  ", path, collapse_indicator), style),
                        Span::styled(format!("+{} ", adds), Style::default().fg(theme.added_fg).bg(stats_bg)),
                        Span::styled(format!("-{} ", dels), Style::default().fg(theme.deleted_fg).bg(stats_bg)),
                    ]))
                }
                DisplayLine::HunkContext { text, line_no, highlights } => {
                    let style = Style::default()
                        .fg(theme.hunk_ctx_fg)
                        .bg(theme.hunk_ctx_bg);
                    let content_spans = build_highlighted_spans(
                        text,
                        highlights,
                        &[],
                        style,
                        false,
                        theme.hunk_ctx_bg,
                        None,
                    );
                    let line_num = format!("{:>4}", line_no);
                    let line_num_style = Style::default()
                        .fg(theme.hunk_ctx_fg)
                        .bg(theme.hunk_ctx_bg)
                        .add_modifier(Modifier::BOLD);
                    ListItem::new(build_context_box(
                        vec![Span::styled(line_num, line_num_style)],
                        content_spans,
                        area.width as usize,
                        style,
                    ))
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
                    let line = vec![
                        Span::styled(format!(" +{} -{} ", additions, deletions), stats_style),
                        Span::styled(format!("starting at line {} ", line_no), style),
                    ];
                    ListItem::new(Line::from(line))
                }
                DisplayLine::HunkEnd { .. } => {
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

                    let annotation_marker = app
                        .annotation_marker_for_line(file_path, side, line_no)
                        .unwrap_or(' ')
                        .to_string();

                    let is_selected = selection_range
                        .map(|(s, e)| idx >= s && idx <= e)
                        .unwrap_or(false);
                    let line_num_fg = match line.kind {
                        LineKind::Addition => theme.added_fg,
                        LineKind::Deletion => theme.deleted_fg,
                        LineKind::Context => theme.line_num,
                    };
                    let line_num_style = if is_current {
                        Style::default().fg(line_num_fg).bg(theme.current_line_bg)
                    } else if is_selected {
                        Style::default().fg(line_num_fg).bg(theme.selection_bg)
                    } else {
                        Style::default().fg(line_num_fg)
                    };
                    let line_num = format!("{:>4}", line_no);

                    let (prefix, mut content_style, prefix_style) = match line.kind {
                        LineKind::Addition => (
                            "+",
                            Style::default().fg(theme.context_fg).bg(theme.added_bg),
                            Style::default().fg(theme.added_fg).bg(theme.added_bg),
                        ),
                        LineKind::Deletion => (
                            "-",
                            Style::default().fg(theme.context_fg).bg(theme.deleted_bg),
                            Style::default().fg(theme.deleted_fg).bg(theme.deleted_bg),
                        ),
                        LineKind::Context => (
                            " ",
                            Style::default().fg(theme.context_fg),
                            Style::default().fg(theme.context_fg),
                        ),
                    };
                    if is_selected && !is_current {
                        content_style = content_style.bg(theme.selection_bg);
                    }

                    // Build spans with syntax highlighting
                    let marker_style = if is_current {
                        Style::default()
                            .fg(theme.annotation_marker)
                            .bg(theme.current_line_bg)
                    } else if is_selected {
                        Style::default()
                            .fg(theme.annotation_marker)
                            .bg(theme.selection_bg)
                    } else {
                        Style::default().fg(theme.annotation_marker)
                    };

                    let prefix_spans = vec![
                        Span::styled(annotation_marker, marker_style),
                        Span::styled(line_num, line_num_style),
                        Span::raw(" "),
                        Span::styled(format!("{} ", prefix), prefix_style),
                    ];

                    let inline_bg = match line.kind {
                        LineKind::Addition => Some(theme.intraline_added_bg),
                        LineKind::Deletion => Some(theme.intraline_deleted_bg),
                        _ => None,
                    };
                    let content_spans = build_highlighted_spans(
                        &line.content,
                        &line.highlights,
                        &line.inline_ranges,
                        content_style,
                        is_current,
                        if is_selected { theme.selection_bg } else { theme.current_line_bg },
                        inline_bg,
                    );

                    let lines = wrap_spans_with_prefix(
                        prefix_spans,
                        content_spans,
                        area.width as usize,
                        content_style,
                    );

                    ListItem::new(lines)
                }
                DisplayLine::Annotation { annotation, orphaned, .. } => {
                    // Annotation in a prominent box - handle multiple lines
                    let resolved = annotation.resolved_at.is_some();
                    let (prefix, style) = match annotation.annotation_type {
                        AnnotationType::Comment => (
                            if resolved {
                                "    ✓ RESOLVED "
                            } else if *orphaned {
                                "    ⚠ ORPHANED "
                            } else {
                                "    💬 "
                            },
                            if resolved {
                                Style::default()
                                    .fg(theme.resolved_fg)
                                    .bg(theme.resolved_bg)
                                    .add_modifier(Modifier::ITALIC)
                            } else if *orphaned {
                                Style::default()
                                    .fg(theme.deleted_fg)
                                    .bg(theme.annotation_bg)
                                    .add_modifier(Modifier::ITALIC)
                            } else {
                                Style::default().fg(theme.annotation_fg).bg(theme.annotation_bg)
                            },
                        ),
                        AnnotationType::Todo => (
                            if resolved {
                                "    ✓ RESOLVED "
                            } else if *orphaned {
                                "    ⚠ ORPHANED "
                            } else {
                                "    📌 "
                            },
                            if resolved {
                                Style::default()
                                    .fg(theme.resolved_fg)
                                    .bg(theme.resolved_bg)
                                    .add_modifier(Modifier::ITALIC)
                            } else if *orphaned {
                                Style::default()
                                    .fg(theme.deleted_fg)
                                    .bg(theme.todo_bg)
                                    .add_modifier(Modifier::ITALIC)
                            } else {
                                Style::default()
                                    .fg(theme.todo_fg)
                                    .bg(theme.todo_bg)
                                    .add_modifier(Modifier::BOLD)
                            },
                        ),
                    };

                    // Split content by newlines and wrap each line with prefix
                    let mut lines: Vec<Line> = Vec::new();
                    let content_lines: Vec<&str> = if annotation.content.is_empty() {
                        vec![""]
                    } else {
                        annotation.content.lines().collect()
                    };
                    for (i, line) in content_lines.into_iter().enumerate() {
                        let line_prefix = if i == 0 {
                            prefix.to_string()
                        } else {
                            "       ".to_string() // Indent continuation lines
                        };
                        let prefix_spans = vec![Span::styled(line_prefix, style)];
                        let content_spans = vec![Span::styled(line.to_string(), style)];
                        let wrapped = wrap_spans_with_prefix(
                            prefix_spans,
                            content_spans,
                            area.width as usize,
                            style,
                        );
                        lines.extend(wrapped);
                    }

                    ListItem::new(lines)
                }
            }
        })
        .collect();

    let dim_diff = app.sidebar_open && app.sidebar_focused;
    let diff_style = if dim_diff {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    };
    // No borders for cleaner look
    let diff_list = List::new(items).style(diff_style);

    f.render_widget(diff_list, area);
}

fn render_diff_side_by_side(f: &mut Frame, app: &mut App, area: Rect, theme: Theme) {
    // Split the area into two columns with a small gap
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(49),
            Constraint::Length(2), // Divider
            Constraint::Percentage(49),
        ])
        .split(area);
    let content_width = columns[0].width.min(columns[2].width) as usize;
    app.content_width = content_width;

    let visible_height = area.height as usize;
    let scroll_offset = app.scroll_offset;

    // Build left (old) and right (new) items
    let mut left_items: Vec<ListItem> = Vec::new();
    let mut right_items: Vec<ListItem> = Vec::new();
    let mut divider_items: Vec<ListItem> = Vec::new();
    let mut push_pair =
        |mut left: Vec<Line<'static>>, mut right: Vec<Line<'static>>, divider_style: Option<Style>| {
        let max_len = left.len().max(right.len()).max(1);
        while left.len() < max_len {
            left.push(Line::from(""));
        }
        while right.len() < max_len {
            right.push(Line::from(""));
        }
        let div_style = divider_style.unwrap_or_default();
        let divider_line = Line::from(Span::styled("  ", div_style));
        let mut divider_lines = Vec::with_capacity(max_len);
        for _ in 0..max_len {
            divider_lines.push(divider_line.clone());
        }
        left_items.push(ListItem::new(left));
        right_items.push(ListItem::new(right));
        divider_items.push(ListItem::new(divider_lines));
    };
    let blank_lines = |count: usize| {
        let mut out = Vec::with_capacity(count.max(1));
        for _ in 0..count.max(1) {
            out.push(Line::from(""));
        }
        out
    };

    let selection_range = app.selection_range();
    for (idx, display_line) in app.display_lines.iter().enumerate().skip(scroll_offset).take(visible_height) {
        let is_current = idx == app.current_line_idx;

        match display_line {
            DisplayLine::Spacer => {
                push_pair(vec![Line::from("")], vec![Line::from("")], None);
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

                let collapse_indicator = if app.collapsed_files.contains(path) { " ▶" } else { "" };

                let header = Line::from(vec![
                    Span::styled(format!(" Δ {}{} ", path, collapse_indicator), style),
                    Span::styled(format!("+{} ", adds), Style::default().fg(theme.added_fg).bg(stats_bg)),
                    Span::styled(format!("-{} ", dels), Style::default().fg(theme.deleted_fg).bg(stats_bg)),
                ]);
                push_pair(vec![header], blank_lines(1), None);
            }
            DisplayLine::HunkContext { text, line_no, highlights } => {
                let style = Style::default()
                    .fg(theme.hunk_ctx_fg)
                    .bg(theme.hunk_ctx_bg);
                let content_spans =
                    build_highlighted_spans(text, highlights, &[], style, false, theme.hunk_ctx_bg, None);
                let line_num = format!("{:>4}", line_no);
                let line_num_style = Style::default()
                    .fg(theme.hunk_ctx_fg)
                    .bg(theme.hunk_ctx_bg)
                    .add_modifier(Modifier::BOLD);
                let lines = build_context_box(
                    vec![Span::styled(line_num, line_num_style)],
                    content_spans,
                    content_width,
                    style,
                );
                let right_fill = blank_lines(lines.len());
                push_pair(lines, right_fill, None);
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
                let line = vec![
                    Span::styled(format!(" +{} -{} ", additions, deletions), stats_style),
                    Span::styled(format!("starting at line {} ", line_no), style),
                ];
                let line = Line::from(line);
                push_pair(vec![line], blank_lines(1), None);
            }
            DisplayLine::HunkEnd { .. } => {
                // End of hunk - subtle bottom border
                let style = Style::default().fg(theme.hunk_border);
                let line = Line::from(Span::styled("╰".to_string() + &"─".repeat(20), style));
                push_pair(vec![line], vec![Line::from("")], None);
            }
            DisplayLine::Diff { line, file_path, .. } => {
                let is_selected = selection_range
                    .map(|(s, e)| idx >= s && idx <= e)
                    .unwrap_or(false);
                let line_no = line.new_line_no.or(line.old_line_no).unwrap_or(0);
                let side = if line.new_line_no.is_some() {
                    Side::New
                } else {
                    Side::Old
                };

                let marker_char = app
                    .annotation_marker_for_line(file_path, side, line_no)
                    .unwrap_or(' ');
                let marker_style = if is_current {
                    Style::default()
                        .fg(theme.annotation_marker)
                        .bg(theme.current_line_bg)
                } else if is_selected {
                    Style::default()
                        .fg(theme.annotation_marker)
                        .bg(theme.selection_bg)
                } else {
                    Style::default().fg(theme.annotation_marker)
                };
                let marker = marker_char.to_string();

                match line.kind {
                    LineKind::Deletion => {
                        let old_no = line
                            .old_line_no
                            .map_or("    ".to_string(), |n| format!("{:>4}", n));
                        let content_style = Style::default().fg(theme.context_fg).bg(theme.deleted_bg);
                        let prefix_style = Style::default().fg(theme.deleted_fg).bg(theme.deleted_bg);
                        let line_num_style = if is_current {
                            Style::default().fg(theme.deleted_fg).bg(theme.current_line_bg)
                        } else if is_selected {
                            Style::default().fg(theme.deleted_fg).bg(theme.selection_bg)
                        } else {
                            Style::default().fg(theme.deleted_fg)
                        };
                        let content_style = if is_selected && !is_current {
                            content_style.bg(theme.selection_bg)
                        } else {
                            content_style
                        };
                        let left_spans = vec![
                            Span::styled(marker.clone(), marker_style),
                            Span::styled(old_no, line_num_style),
                            Span::styled("- ", prefix_style),
                        ];
                        let content_spans = build_highlighted_spans(
                            &line.content,
                            &line.highlights,
                            &line.inline_ranges,
                            content_style,
                            is_current,
                            if is_selected { theme.selection_bg } else { theme.current_line_bg },
                            Some(theme.intraline_deleted_bg),
                        );
                        let lines = wrap_spans_with_prefix(left_spans, content_spans, content_width, content_style);
                        push_pair(lines, Vec::new(), None);
                    }
                    LineKind::Addition => {
                        let new_no = line
                            .new_line_no
                            .map_or("    ".to_string(), |n| format!("{:>4}", n));
                        let content_style = Style::default().fg(theme.context_fg).bg(theme.added_bg);
                        let prefix_style = Style::default().fg(theme.added_fg).bg(theme.added_bg);
                        let line_num_style = if is_current {
                            Style::default().fg(theme.added_fg).bg(theme.current_line_bg)
                        } else if is_selected {
                            Style::default().fg(theme.added_fg).bg(theme.selection_bg)
                        } else {
                            Style::default().fg(theme.added_fg)
                        };
                        let content_style = if is_selected && !is_current {
                            content_style.bg(theme.selection_bg)
                        } else {
                            content_style
                        };
                        let right_spans = vec![
                            Span::styled(marker.clone(), marker_style),
                            Span::styled(new_no, line_num_style),
                            Span::styled("+ ", prefix_style),
                        ];
                        let content_spans = build_highlighted_spans(
                            &line.content,
                            &line.highlights,
                            &line.inline_ranges,
                            content_style,
                            is_current,
                            if is_selected { theme.selection_bg } else { theme.current_line_bg },
                            Some(theme.intraline_added_bg),
                        );
                        let lines = wrap_spans_with_prefix(right_spans, content_spans, content_width, content_style);
                        push_pair(Vec::new(), lines, None);
                    }
                    LineKind::Context => {
                        let old_no = line
                            .old_line_no
                            .map_or("    ".to_string(), |n| format!("{:>4}", n));
                        let new_no = line
                            .new_line_no
                            .map_or("    ".to_string(), |n| format!("{:>4}", n));
                        let content_style = Style::default().fg(theme.context_fg);
                        let line_num_style = if is_current {
                            Style::default().fg(theme.line_num).bg(theme.current_line_bg)
                        } else if is_selected {
                            Style::default().fg(theme.line_num).bg(theme.selection_bg)
                        } else {
                            Style::default().fg(theme.line_num)
                        };
                        let content_style = if is_selected && !is_current {
                            content_style.bg(theme.selection_bg)
                        } else {
                            content_style
                        };
                        let left_spans = vec![
                            Span::styled(marker.clone(), marker_style),
                            Span::styled(old_no, line_num_style),
                            Span::styled("  ", content_style),
                        ];
                        let content_spans = build_highlighted_spans(
                            &line.content,
                            &line.highlights,
                            &line.inline_ranges,
                            content_style,
                            is_current,
                            if is_selected { theme.selection_bg } else { theme.current_line_bg },
                            None,
                        );
                        let left_lines =
                            wrap_spans_with_prefix(left_spans, content_spans, content_width, content_style);

                        let right_spans = vec![
                            Span::styled(marker.clone(), marker_style),
                            Span::styled(new_no, line_num_style),
                            Span::styled("  ", content_style),
                        ];
                        let content_spans = build_highlighted_spans(
                            &line.content,
                            &line.highlights,
                            &line.inline_ranges,
                            content_style,
                            is_current,
                            if is_selected { theme.selection_bg } else { theme.current_line_bg },
                            None,
                        );
                        let right_lines =
                            wrap_spans_with_prefix(right_spans, content_spans, content_width, content_style);
                        push_pair(left_lines, right_lines, None);
                    }
                }
            }
            DisplayLine::Annotation { annotation, orphaned, .. } => {
                // Annotation in a prominent box - handle multiple lines
                let resolved = annotation.resolved_at.is_some();
                let (prefix, style) = match annotation.annotation_type {
                    AnnotationType::Comment => (
                        if resolved {
                            "   ✓ RESOLVED "
                        } else if *orphaned {
                            "   ⚠ ORPHANED "
                        } else {
                            "   💬 "
                        },
                        if resolved {
                            Style::default()
                                .fg(theme.resolved_fg)
                                .bg(theme.resolved_bg)
                                .add_modifier(Modifier::ITALIC)
                        } else if *orphaned {
                            Style::default()
                                .fg(theme.deleted_fg)
                                .bg(theme.annotation_bg)
                                .add_modifier(Modifier::ITALIC)
                        } else {
                            Style::default().fg(theme.annotation_fg).bg(theme.annotation_bg)
                        },
                    ),
                    AnnotationType::Todo => (
                        if resolved {
                            "   ✓ RESOLVED "
                        } else if *orphaned {
                            "   ⚠ ORPHANED "
                        } else {
                            "   📌 "
                        },
                        if resolved {
                            Style::default()
                                .fg(theme.resolved_fg)
                                .bg(theme.resolved_bg)
                                .add_modifier(Modifier::ITALIC)
                        } else if *orphaned {
                            Style::default()
                                .fg(theme.deleted_fg)
                                .bg(theme.todo_bg)
                                .add_modifier(Modifier::ITALIC)
                        } else {
                            Style::default()
                                .fg(theme.todo_fg)
                                .bg(theme.todo_bg)
                                .add_modifier(Modifier::BOLD)
                        },
                    ),
                };

                // Show annotation on the appropriate side
                match annotation.side {
                    Side::Old => {
                        let mut lines: Vec<Line> = Vec::new();
                        let content_lines: Vec<&str> = if annotation.content.is_empty() {
                            vec![""]
                        } else {
                            annotation.content.lines().collect()
                        };
                        for (i, line) in content_lines.into_iter().enumerate() {
                            let line_prefix = if i == 0 {
                                prefix.to_string()
                            } else {
                                "      ".to_string()
                            };
                            let prefix_spans = vec![Span::styled(line_prefix, style)];
                            let content_spans = vec![Span::styled(line.to_string(), style)];
                            let wrapped =
                                wrap_spans_with_prefix(prefix_spans, content_spans, content_width, style);
                            lines.extend(wrapped);
                        }
                        push_pair(lines, Vec::new(), None);
                    }
                    Side::New => {
                        let mut right_lines: Vec<Line> = Vec::new();
                        let content_lines: Vec<&str> = if annotation.content.is_empty() {
                            vec![""]
                        } else {
                            annotation.content.lines().collect()
                        };
                        for (i, line) in content_lines.into_iter().enumerate() {
                            let line_prefix = if i == 0 {
                                prefix.to_string()
                            } else {
                                "      ".to_string()
                            };
                            let prefix_spans = vec![Span::styled(line_prefix, style)];
                            let content_spans = vec![Span::styled(line.to_string(), style)];
                            let wrapped =
                                wrap_spans_with_prefix(prefix_spans, content_spans, content_width, style);
                            right_lines.extend(wrapped);
                        }
                        push_pair(Vec::new(), right_lines, None);
                    }
                }
            }
        }
    }

    let dim_diff = app.sidebar_open && app.sidebar_focused;
    let diff_style = if dim_diff {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    };
    // No borders, cleaner look
    let left_list = List::new(left_items).style(diff_style);
    let divider_list = List::new(divider_items).style(diff_style);
    let right_list = List::new(right_items).style(diff_style);

    f.render_widget(left_list, columns[0]);
    f.render_widget(divider_list, columns[1]);
    f.render_widget(right_list, columns[2]);
}

fn render_status(f: &mut Frame, app: &App, area: Rect, theme: Theme) {
    match &app.mode {
        Mode::Normal => {
            let mode_label = match app.diff_mode {
                DiffMode::Unstaged => "[unstaged] ",
                DiffMode::Staged => "[staged] ",
                _ => "",
            };
            let ai_running = app.ai_jobs.iter().filter(|j| matches!(j.status, AiStatus::Running)).count();
            let ai_suffix = if ai_running > 0 {
                format!("  AI: running ({})", ai_running)
            } else {
                String::new()
            };
            let loading_suffix = if app.diff_loading {
                format!("  Loading… ({})", app.files.len())
            } else {
                String::new()
            };
            let shortcuts = " j/k:nav  n/N:chunk  c:collapse  B:sidebar  a:add  e:edit  r:resolve  ?:help";
            let content = if let Some(msg) = &app.message {
                format!("{}{}{}{}", mode_label, msg, ai_suffix, loading_suffix)
            } else {
                format!("{}{}{}{}", mode_label, shortcuts, ai_suffix, loading_suffix)
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

            let input_with_cursor = app.render_input_with_cursor();

            let input = Paragraph::new(input_with_cursor)
                .style(Style::default().fg(theme.header_fg).bg(theme.surface_alt))
                .block(Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border))
                    .title(title))
                .wrap(Wrap { trim: false });

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
        Mode::GotoLine => {
            let query = app.goto_line_input.as_str();
            let content = format!(" Go to line: {}_  (Enter: go, Esc: cancel)", query);
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

fn render_sidebar(f: &mut Frame, app: &mut App, area: Rect, theme: Theme) {
    let entries = app.sidebar_entries();
    if entries.is_empty() {
        let empty = Paragraph::new(" No changes")
            .style(Style::default().fg(theme.status_fg).bg(theme.status_bg))
            .block(Block::default().borders(Borders::RIGHT).border_style(Style::default().fg(theme.border)));
        f.render_widget(empty, area);
        return;
    }

    if app.sidebar_index >= entries.len() {
        app.sidebar_index = entries.len().saturating_sub(1);
    }

    let visible_height = area.height as usize;
    if app.sidebar_index < app.sidebar_scroll {
        app.sidebar_scroll = app.sidebar_index;
    } else if app.sidebar_index >= app.sidebar_scroll + visible_height {
        app.sidebar_scroll = app.sidebar_index.saturating_sub(visible_height.saturating_sub(1));
    }

    let items: Vec<ListItem> = entries
        .iter()
        .enumerate()
        .skip(app.sidebar_scroll)
        .take(visible_height)
        .map(|(idx, entry)| {
            let status_char = match entry.status {
                FileStatus::Added => "A",
                FileStatus::Deleted => "D",
                FileStatus::Renamed => "R",
                _ => "M",
            };
            let status_color = match entry.status {
                FileStatus::Added => theme.added_fg,
                FileStatus::Deleted => theme.deleted_fg,
                FileStatus::Renamed => theme.hunk_fg,
                _ => theme.header_fg,
            };
            let is_selected = idx == app.sidebar_index;
            let mut style = Style::default().fg(theme.status_fg);
            if is_selected {
                style = if app.sidebar_focused {
                    style.bg(theme.current_line_bg)
                } else {
                    style.bg(theme.selection_bg)
                };
            }
            if !app.sidebar_focused {
                style = style.add_modifier(Modifier::DIM);
            }
            let line = Line::from(vec![
                Span::styled(format!(" {} ", status_char), Style::default().fg(status_color)),
                Span::styled(entry.path.clone(), style),
            ]);
            ListItem::new(line)
        })
        .collect();

    let title = if app.sidebar_focused { " Files • focused " } else { " Files (b to focus) " };
    let mut border_style = Style::default().fg(theme.border);
    if app.sidebar_focused {
        border_style = border_style.add_modifier(Modifier::BOLD);
    } else {
        border_style = border_style.add_modifier(Modifier::DIM);
    }
    let list_style = if app.sidebar_focused {
        Style::default().fg(theme.status_fg)
    } else {
        Style::default()
            .fg(theme.status_fg)
            .add_modifier(Modifier::DIM)
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(border_style)
                .title(title),
        )
        .style(list_style);
    f.render_widget(list, area);
}

fn build_context_box(
    prefix_spans: Vec<Span<'static>>,
    content_spans: Vec<Span<'static>>,
    width: usize,
    style: Style,
) -> Vec<Line<'static>> {
    if width < 4 {
        let mut line = prefix_spans;
        line.extend(content_spans);
        return vec![Line::from(line)];
    }

    let top = format!("┌{}┐", "─".repeat(width.saturating_sub(2)));
    let bottom = format!("└{}┘", "─".repeat(width.saturating_sub(2)));

    let available = width.saturating_sub(4);
    let mut inner: Vec<Span<'static>> = Vec::new();
    inner.extend(prefix_spans);
    if !inner.is_empty() {
        inner.push(Span::styled(" ", style));
    }
    inner.extend(content_spans);
    let trimmed = truncate_spans(&inner, available);

    let mut content_line = vec![Span::styled("│ ", style)];
    content_line.extend(trimmed);
    let used: usize = content_line.iter().map(span_width).sum();
    let target = width.saturating_sub(1);
    if used < target {
        let pad = " ".repeat(target - used);
        content_line.push(Span::styled(pad, style));
    }
    content_line.push(Span::styled("│", style));

    vec![
        Line::from(Span::styled(top, style)),
        Line::from(content_line),
        Line::from(Span::styled(bottom, style)),
    ]
}

fn truncate_spans(spans: &[Span<'static>], max_width: usize) -> Vec<Span<'static>> {
    if max_width == 0 {
        return Vec::new();
    }
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut remaining = max_width;
    for span in spans {
        if remaining == 0 {
            break;
        }
        let w = span_width(span);
        if w <= remaining {
            out.push(span.clone());
            remaining -= w;
        } else {
            let (head, _) = split_by_width(&span.content, remaining);
            if !head.is_empty() {
                out.push(Span::styled(head, span.style));
            }
            break;
        }
    }
    out
}

fn render_help(f: &mut Frame, app: &App, area: Rect, theme: Theme) {
    let help = Paragraph::new(HELP_TEXT.join("\n"))
        .style(Style::default().fg(theme.help_fg).bg(theme.help_bg))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Help ")
                .style(Style::default().bg(theme.help_bg))
                .border_style(Style::default().fg(theme.border)),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.help_scroll as u16, 0));

    f.render_widget(Clear, area);
    f.render_widget(help, area);
}

fn run_ai_process<F>(
    target: &AiTarget,
    prompt: &str,
    repo_path: &PathBuf,
    mut on_output: F,
) -> Result<()>
where
    F: FnMut(&str),
{
    let mut cmd = match target {
        AiTarget::Codex => {
            let mut cmd = Command::new("codex");
            cmd.arg("exec").arg(prompt);
            cmd
        }
        AiTarget::Claude => {
            let mut cmd = Command::new("claude");
            cmd.arg("-p")
                .arg("--verbose")
                .arg("--output-format")
                .arg("stream-json")
                .arg("--include-partial-messages")
                .arg("--")
                .arg(prompt);
            cmd
        }
    };

    cmd.current_dir(repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| anyhow!(e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (tx, rx) = mpsc::channel::<String>();

    if let Some(out) = stdout {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(out);
            let mut buf = String::new();
            while reader.read_line(&mut buf).is_ok() {
                if buf.is_empty() {
                    break;
                }
                let _ = tx.send(buf.clone());
                buf.clear();
            }
        });
    }

    if let Some(err) = stderr {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(err);
            let mut buf = String::new();
            while reader.read_line(&mut buf).is_ok() {
                if buf.is_empty() {
                    break;
                }
                let _ = tx.send(buf.clone());
                buf.clear();
            }
        });
    }

    drop(tx);

    for line in rx.iter() {
        on_output(&line);
    }

    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("AI process failed"))
    }
}

fn extract_claude_text(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return None;
    };

    let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if event_type == "content_block_delta" {
        if let Some(text) = value
            .get("delta")
            .and_then(|d| d.get("text"))
            .and_then(|t| t.as_str())
        {
            return Some(text.to_string());
        }
    }
    if event_type == "message_delta" {
        if let Some(text) = value.get("text").and_then(|t| t.as_str()) {
            return Some(text.to_string());
        }
    }
    if let Some(text) = value.get("text").and_then(|t| t.as_str()) {
        return Some(text.to_string());
    }
    None
}

fn read_working_file_at(repo_path: &PathBuf, path: &str) -> Option<String> {
    let full_path = repo_path.join(path);
    std::fs::read(&full_path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
}

fn git_show_at(repo_path: &PathBuf, spec: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("show")
        .arg(spec)
        .current_dir(repo_path)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        None
    }
}

fn load_new_file_content_for_highlight(
    repo_path: &PathBuf,
    diff_mode: &DiffMode,
    file: &DiffFile,
    file_path: &str,
) -> Option<String> {
    if matches!(file.status, FileStatus::Deleted) {
        return None;
    }
    let path = file
        .new_path
        .as_ref()
        .or(file.old_path.as_ref())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| file_path.to_string());

    match diff_mode {
        DiffMode::Unstaged => read_working_file_at(repo_path, &path),
        DiffMode::Staged => git_show_at(repo_path, &format!(":{}", path)),
        DiffMode::WorkingTree { .. } => read_working_file_at(repo_path, &path),
        DiffMode::Commits { to, .. } => git_show_at(repo_path, &format!("{}:{}", to, path)),
        DiffMode::MergeBase { to, .. } => git_show_at(repo_path, &format!("{}:{}", to, path)),
        DiffMode::ExternalDiff { .. } => read_working_file_at(repo_path, &path),
    }
}

fn build_highlight_map_with(
    highlighter: &mut SyntaxHighlighter,
    content: &str,
    file_path: &str,
) -> Vec<Vec<HighlightRange>> {
    let per_line = highlighter.highlight_file(content, file_path);
    per_line
        .into_iter()
        .map(|line_hl| {
            line_hl
                .into_iter()
                .map(|h| HighlightRange {
                    start: h.start,
                    end: h.end,
                    style: h.style,
                })
                .collect()
        })
        .collect()
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
    orphaned: bool,
    resolved: bool,
}

#[derive(Clone)]
struct SidebarEntry {
    file_idx: usize,
    path: String,
    status: FileStatus,
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
            let is_orphaned = entry.orphaned;
            let resolved = entry.resolved;

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
            if resolved {
                label.push_str("  (resolved)");
            } else if is_orphaned {
                label.push_str("  (orphaned)");
            }

            let mut style = if resolved {
                Style::default().fg(theme.line_num)
            } else if is_orphaned {
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

fn render_ai_pane(f: &mut Frame, app: &mut App, theme: Theme) {
    let area = centered_rect(96, 60, f.area());
    let mut lines: Vec<Line> = Vec::new();

    if app.ai_jobs.is_empty() {
        lines.push(Line::from(Span::styled(
            "No AI activity",
            Style::default().fg(theme.help_fg),
        )));
    } else {
        let mut tabs: Vec<Span> = Vec::new();
        for (idx, job) in app.ai_jobs.iter().enumerate() {
            let status = match job.status {
                AiStatus::Running => "…",
                AiStatus::Done { ok } => {
                    if ok { "✓" } else { "!" }
                }
            };
            let label = format!(" {}#{}{} ", status, job.annotation_id, if job.annotation_type == AnnotationType::Todo { "T" } else { "C" });
            let style = if idx == app.ai_selected_idx {
                Style::default()
                    .fg(theme.header_match_fg)
                    .bg(theme.header_match_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.help_fg)
            };
            tabs.push(Span::styled(label, style));
        }
        lines.push(Line::from(tabs));
        lines.push(Line::from(""));

        let idx = app.ai_selected_idx.min(app.ai_jobs.len().saturating_sub(1));
        let job = &app.ai_jobs[idx];
        let status = match job.status {
            AiStatus::Running => "running",
            AiStatus::Done { ok } => if ok { "done" } else { "error" },
        };
        let header = format!(
            "[{}] #{} {}:{} ({})",
            status, job.annotation_id, job.file_path, job.line, job.annotation_type.as_str()
        );
        lines.push(Line::from(Span::styled(
            header,
            Style::default().fg(theme.header_fg).add_modifier(Modifier::BOLD),
        )));

        let mut output = job.output.replace('\t', " ");
        if output.len() > 2000 {
            output = format!("...{}", &output[output.len() - 2000..]);
        }
        let tail_lines: Vec<&str> = output.lines().rev().take(20).collect();
        for line in tail_lines.iter().rev() {
            lines.push(Line::from(Span::styled(
                (*line).to_string(),
                Style::default().fg(theme.help_fg),
            )));
        }
    }

    let pane = Paragraph::new(lines)
        .style(Style::default().bg(theme.help_bg))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" AI (P toggle, [/] switch) ")
                .border_style(Style::default().fg(theme.border)),
        );

    f.render_widget(Clear, area);
    f.render_widget(pane, area);
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
/// Build spans with syntax highlighting for a diff line
fn build_highlighted_spans(
    content: &str,
    highlights: &[HighlightRange],
    inline_ranges: &[InlineRange],
    base_style: Style,
    is_current: bool,
    current_line_bg: Color,
    inline_bg: Option<Color>,
) -> Vec<Span<'static>> {
    let base_style = if is_current {
        base_style
            .bg(current_line_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        base_style
    };

    if highlights.is_empty() && inline_ranges.is_empty() {
        return vec![Span::styled(content.to_string(), base_style)];
    }

    let content_bytes = content.as_bytes();
    let mut boundaries: Vec<usize> = Vec::new();
    boundaries.push(0);
    boundaries.push(content_bytes.len());
    for h in highlights {
        boundaries.push(h.start.min(content_bytes.len()));
        boundaries.push(h.end.min(content_bytes.len()));
    }
    for r in inline_ranges {
        boundaries.push(r.start.min(content_bytes.len()));
        boundaries.push(r.end.min(content_bytes.len()));
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut h_idx = 0usize;
    let mut sorted_highlights: Vec<HighlightRange> = highlights.to_vec();
    sorted_highlights.sort_by_key(|h| h.start);
    let mut sorted_inline: Vec<InlineRange> = inline_ranges.to_vec();
    sorted_inline.sort_by_key(|r| r.start);
    let mut i_idx = 0usize;

    for window in boundaries.windows(2) {
        let start = window[0];
        let end = window[1];
        if start >= end {
            continue;
        }
        while h_idx < sorted_highlights.len() && sorted_highlights[h_idx].end <= start {
            h_idx += 1;
        }
        while i_idx < sorted_inline.len() && sorted_inline[i_idx].end <= start {
            i_idx += 1;
        }

        let mut style = base_style;
        if h_idx < sorted_highlights.len() {
            let h = &sorted_highlights[h_idx];
            if h.start <= start && h.end >= end {
                style = apply_text_style(base_style, h.style, is_current);
            }
        }
        if let Some(bg) = inline_bg {
            if i_idx < sorted_inline.len() {
                let r = &sorted_inline[i_idx];
                if r.start <= start && r.end >= end {
                    style = style.bg(bg).add_modifier(Modifier::BOLD);
                }
            }
        }

        if let Ok(segment) = std::str::from_utf8(&content_bytes[start..end]) {
            spans.push(Span::styled(segment.to_string(), style));
        }
    }

    spans
}

fn apply_text_style(base: Style, style: crate::syntax::TextStyle, is_current: bool) -> Style {
    let mut s = base.fg(Color::Rgb(style.fg.0, style.fg.1, style.fg.2));
    if style.bold {
        s = s.add_modifier(Modifier::BOLD);
    }
    if style.italic {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if style.underline {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    if is_current {
        s = s.add_modifier(Modifier::BOLD);
    }
    s
}

fn span_width(span: &Span) -> usize {
    span.content.width()
}

fn compute_inline_ranges(old: &str, new: &str) -> (Vec<InlineRange>, Vec<InlineRange>) {
    let diff = TextDiff::from_chars(old, new);
    let mut old_pos = 0usize;
    let mut new_pos = 0usize;
    let mut old_ranges = Vec::new();
    let mut new_ranges = Vec::new();

    for change in diff.iter_all_changes() {
        let len = change.value().len();
        if len == 0 {
            continue;
        }
        match change.tag() {
            ChangeTag::Delete => {
                old_ranges.push(InlineRange {
                    start: old_pos,
                    end: old_pos + len,
                });
                old_pos += len;
            }
            ChangeTag::Insert => {
                new_ranges.push(InlineRange {
                    start: new_pos,
                    end: new_pos + len,
                });
                new_pos += len;
            }
            ChangeTag::Equal => {
                old_pos += len;
                new_pos += len;
            }
        }
    }

    (merge_inline_ranges(old_ranges), merge_inline_ranges(new_ranges))
}

fn line_similarity(old: &str, new: &str) -> f32 {
    TextDiff::from_chars(old, new).ratio()
}

fn merge_inline_ranges(mut ranges: Vec<InlineRange>) -> Vec<InlineRange> {
    if ranges.is_empty() {
        return ranges;
    }
    ranges.sort_by_key(|r| r.start);
    let mut merged = Vec::new();
    let mut current = ranges.remove(0);
    for r in ranges {
        if r.start <= current.end {
            current.end = current.end.max(r.end);
        } else {
            merged.push(current);
            current = r;
        }
    }
    merged.push(current);
    merged
}

fn compute_intraline_ranges(lines: &[DiffLine]) -> Vec<Vec<InlineRange>> {
    let mut per_line: Vec<Vec<InlineRange>> = vec![Vec::new(); lines.len()];
    let mut i = 0usize;
    while i < lines.len() {
        if lines[i].kind == LineKind::Deletion {
            let del_start = i;
            while i < lines.len() && lines[i].kind == LineKind::Deletion {
                i += 1;
            }
            let add_start = i;
            while i < lines.len() && lines[i].kind == LineKind::Addition {
                i += 1;
            }
            let del_count = add_start - del_start;
            let add_count = i - add_start;
            if del_count == 0 || add_count == 0 {
                continue;
            }

            let mut candidates: Vec<(f32, usize, usize, usize)> = Vec::new();
            for d in del_start..add_start {
                let old_line = lines[d].content.as_str();
                for a in add_start..i {
                    let new_line = lines[a].content.as_str();
                    let score = line_similarity(old_line, new_line);
                    if score >= INTRALINE_PAIR_RATIO_THRESHOLD {
                        let rel_d = d - del_start;
                        let rel_a = a - add_start;
                        let distance = rel_d.abs_diff(rel_a);
                        candidates.push((score, distance, d, a));
                    }
                }
            }

            candidates.sort_by(|a, b| {
                b.0.partial_cmp(&a.0)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| a.1.cmp(&b.1))
                    .then_with(|| a.2.cmp(&b.2))
                    .then_with(|| a.3.cmp(&b.3))
            });

            let mut used_del = vec![false; del_count];
            let mut used_add = vec![false; add_count];

            for (_score, _distance, d, a) in candidates {
                let rel_d = d - del_start;
                let rel_a = a - add_start;
                if used_del[rel_d] || used_add[rel_a] {
                    continue;
                }
                let (del_ranges, add_ranges) =
                    compute_inline_ranges(&lines[d].content, &lines[a].content);
                per_line[d] = del_ranges;
                per_line[a] = add_ranges;
                used_del[rel_d] = true;
                used_add[rel_a] = true;
            }
        } else {
            i += 1;
        }
    }
    per_line
}

fn split_by_width(s: &str, max_width: usize) -> (String, String) {
    if max_width == 0 || s.is_empty() {
        return (String::new(), s.to_string());
    }
    let mut width = 0usize;
    let mut split_idx = 0usize;
    for (idx, ch) in s.char_indices() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        width += ch_width;
        split_idx = idx + ch.len_utf8();
    }
    let head = s[..split_idx].to_string();
    let tail = s[split_idx..].to_string();
    (head, tail)
}

fn wrap_spans(content_spans: &[Span<'static>], max_width: usize) -> Vec<Vec<Span<'static>>> {
    if max_width == 0 {
        return vec![Vec::new()];
    }

    let mut lines: Vec<Vec<Span>> = vec![Vec::new()];
    let mut line_width = 0usize;

    for span in content_spans {
        let mut text = span.content.to_string();
        while !text.is_empty() {
            let remaining = max_width.saturating_sub(line_width);
            if remaining == 0 {
                lines.push(Vec::new());
                line_width = 0;
                continue;
            }
            let (head, tail) = split_by_width(&text, remaining);
            if !head.is_empty() {
                line_width += head.width();
                lines
                    .last_mut()
                    .unwrap()
                    .push(Span::styled(head, span.style));
            }
            text = tail;
            if !text.is_empty() {
                lines.push(Vec::new());
                line_width = 0;
            }
        }
    }

    lines
}

fn wrap_spans_with_prefix(
    prefix_spans: Vec<Span<'static>>,
    content_spans: Vec<Span<'static>>,
    max_width: usize,
    continuation_style: Style,
) -> Vec<Line<'static>> {
    let prefix_width: usize = prefix_spans.iter().map(span_width).sum();
    let content_width = max_width.saturating_sub(prefix_width);
    let wrapped = wrap_spans(&content_spans, content_width.max(1));

    let mut lines: Vec<Line> = Vec::new();
    for (idx, spans) in wrapped.into_iter().enumerate() {
        if idx == 0 {
            let mut all = prefix_spans.clone();
            all.extend(spans);
            lines.push(Line::from(all));
        } else {
            let indent = " ".repeat(prefix_width);
            let mut all = vec![Span::styled(indent, continuation_style)];
            all.extend(spans);
            lines.push(Line::from(all));
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(prefix_spans));
    }

    lines
}

fn wrapped_line_count(text: &str, max_width: usize) -> usize {
    if max_width == 0 {
        return 1;
    }
    let mut count = 0usize;
    for line in text.split('\n') {
        let width = UnicodeWidthStr::width(line);
        let lines = if width == 0 {
            1
        } else {
            (width + max_width - 1) / max_width
        };
        count += lines;
    }
    if count == 0 { 1 } else { count }
}
