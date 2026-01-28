//! TUI layer using ratatui and crossterm
//!
//! Provides interactive diff viewing with annotation support.

use crate::config::{AiTarget, Config};
use crate::diff::{
    DiffEngine, DiffFile, DiffHunk, DiffLine, DiffMode, FileStatus, HighlightRange, LineKind,
};
use crate::storage::{Annotation, AnnotationType, Side, Storage};
use crate::syntax::SyntaxHighlighter;
use anyhow::{anyhow, Result};
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
use std::io::{self, BufRead, Stdout, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const REATTACH_CONTEXT_LINES: usize = 2;
const INITIAL_HUNK_HIGHLIGHT_COUNT: usize = 5;

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
    "    v         Toggle side-by-side view",
    "    s         Stage/unstage current hunk",
    "    u         Toggle staged/unstaged view",
    "    R         Reload diff",
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
    "",
    "  Other:",
    "    ?         Toggle this help",
    "    q         Quit",
    "",
    "  In annotation mode:",
    "    Enter     Save annotation",
    "    Ctrl+j    Add newline",
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
    display_lines: Vec<DisplayLine>,
    current_line_idx: usize,
    scroll_offset: usize,

    // All annotations keyed by (file_path, side, line_no)
    all_annotations: Vec<Annotation>,

    // Syntax highlighting
    syntax_highlighter: SyntaxHighlighter,
    syntax_cache_old: HashMap<String, Vec<Vec<HighlightRange>>>,
    syntax_cache_new: HashMap<String, Vec<Vec<HighlightRange>>>,
    eager_highlight_files: HashSet<String>,
    highlight_pending: HashSet<String>,
    highlight_generation: u64,
    highlight_rx: Receiver<HighlightEvent>,
    highlight_tx: Sender<HighlightEvent>,

    // UI state
    mode: Mode,
    annotation_input: String,
    annotation_cursor: usize,
    annotation_type: AnnotationType,
    message: Option<String>,
    show_help: bool,
    help_scroll: usize,
    help_visible_height: usize,
    show_annotations: bool,
    side_by_side: bool,
    visible_height: usize,
    expanded_file: Option<usize>, // When Some, only show this file
    collapsed_files: HashSet<String>,
    collapsed_files_unstaged: HashSet<String>,
    collapsed_files_staged: HashSet<String>,
    collapsed_files_other: HashSet<String>,
    // Position to restore when collapsing expanded view
    pre_expand_position: Option<(usize, usize)>, // (line_idx, scroll_offset)
    annotation_list_idx: usize,

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
}

#[derive(Debug, Clone)]
enum AiStatus {
    Running,
    Done { ok: bool },
}

#[derive(Debug, Clone)]
enum HighlightEvent {
    Ready {
        generation: u64,
        file_key: String,
        old_key: String,
        new_key: String,
        old_map: Vec<Vec<HighlightRange>>,
        new_map: Vec<Vec<HighlightRange>>,
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
        let (highlight_tx, highlight_rx) = mpsc::channel();

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
            display_lines: Vec::new(),
            current_line_idx: 0,
            scroll_offset: 0,
            all_annotations: Vec::new(),
            syntax_highlighter: SyntaxHighlighter::new(syntax_theme.as_deref())?,
            syntax_cache_old: HashMap::new(),
            syntax_cache_new: HashMap::new(),
            eager_highlight_files: HashSet::new(),
            highlight_pending: HashSet::new(),
            highlight_generation: 0,
            highlight_rx,
            highlight_tx,
            mode: Mode::Normal,
            annotation_input: String::new(),
            annotation_cursor: 0,
            annotation_type: AnnotationType::Comment,
            message: None,
            show_help: false,
            help_scroll: 0,
            help_visible_height: 0,
            show_annotations,
            side_by_side,
            visible_height: 20, // Will be updated on first render
            expanded_file: None,
            collapsed_files: HashSet::new(),
            collapsed_files_unstaged: HashSet::new(),
            collapsed_files_staged: HashSet::new(),
            collapsed_files_other: HashSet::new(),
            pre_expand_position: None,
            search: None,
            annotation_list_idx: 0,
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

    fn highlight_file_keys_for_first_hunks(&self, max_hunks: usize) -> Vec<String> {
        let mut keys: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut hunk_count = 0usize;

        for file in &self.files {
            let (file_path, _, _) = Self::file_highlight_keys(file);
            if seen.insert(file_path.clone()) {
                keys.push(file_path.clone());
            }

            if !file.hunks.is_empty() {
                hunk_count += file.hunks.len();
            }
            if hunk_count >= max_hunks {
                break;
            }
        }

        if keys.is_empty() {
            if let Some(file) = self.files.first() {
                let (file_path, _, _) = Self::file_highlight_keys(file);
                keys.push(file_path);
            }
        }

        keys
    }

    fn prepare_lazy_highlighting(&mut self) {
        if !self.config.syntax_highlighting {
            return;
        }

        let eager_files = self.highlight_file_keys_for_first_hunks(INITIAL_HUNK_HIGHLIGHT_COUNT);
        self.eager_highlight_files = eager_files.iter().cloned().collect();

        let mut jobs: Vec<(String, String, String, DiffFile)> = Vec::new();
        let files: Vec<DiffFile> = self.files.clone();
        for file in files {
            let (file_key, old_key, new_key) = Self::file_highlight_keys(&file);
            if self.eager_highlight_files.contains(&file_key) {
                let (old_map, new_map) = self.load_highlight_maps(
                    &file,
                    if old_key.is_empty() { None } else { Some(old_key.as_str()) },
                    if new_key.is_empty() { None } else { Some(new_key.as_str()) },
                );
                if !old_key.is_empty() {
                    self.syntax_cache_old.insert(old_key.clone(), old_map);
                }
                if !new_key.is_empty() {
                    self.syntax_cache_new.insert(new_key.clone(), new_map);
                }
            } else {
                if self.highlight_pending.insert(file_key.clone()) {
                    jobs.push((file_key, old_key, new_key, file));
                }
            }
        }

        if jobs.is_empty() {
            return;
        }

        let generation = self.highlight_generation;
        let repo_path = self.repo_path.clone();
        let diff_mode = self.diff_mode.clone();
        let theme_name = self.config.syntax_theme.clone();
        let tx = self.highlight_tx.clone();

        thread::spawn(move || {
            let mut highlighter = match SyntaxHighlighter::new(theme_name.as_deref()) {
                Ok(h) => h,
                Err(_) => return,
            };
            for (file_key, old_key, new_key, file) in jobs {
                let (old_map, new_map) = compute_highlight_maps_for_file(
                    &mut highlighter,
                    &repo_path,
                    &diff_mode,
                    &file,
                    if old_key.is_empty() { None } else { Some(old_key.as_str()) },
                    if new_key.is_empty() { None } else { Some(new_key.as_str()) },
                );
                let _ = tx.send(HighlightEvent::Ready {
                    generation,
                    file_key,
                    old_key,
                    new_key,
                    old_map,
                    new_map,
                });
            }
        });
    }

    fn handle_highlight_event(&mut self, evt: HighlightEvent) -> Result<()> {
        match evt {
            HighlightEvent::Ready {
                generation,
                file_key,
                old_key,
                new_key,
                old_map,
                new_map,
            } => {
                if generation != self.highlight_generation {
                    return Ok(());
                }
                if !old_key.is_empty() {
                    self.syntax_cache_old.insert(old_key, old_map);
                }
                if !new_key.is_empty() {
                    self.syntax_cache_new.insert(new_key, new_map);
                }
                self.highlight_pending.remove(&file_key);
                self.build_display_lines();
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

            if self.collapsed_files.contains(&file_path) && expanded_file.is_none() {
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

    fn cached_highlight_maps(
        &mut self,
        file: &DiffFile,
        old_path: Option<&str>,
        new_path: Option<&str>,
        allow_compute: bool,
    ) -> (Vec<Vec<HighlightRange>>, Vec<Vec<HighlightRange>>) {
        if !self.config.syntax_highlighting {
            return (Vec::new(), Vec::new());
        }

        let old_key = old_path.unwrap_or("").to_string();
        let new_key = new_path.or(old_path).unwrap_or("").to_string();

        let has_old = !old_key.is_empty() && self.syntax_cache_old.contains_key(&old_key);
        let has_new = !new_key.is_empty() && self.syntax_cache_new.contains_key(&new_key);

        if (!has_old || !has_new) && allow_compute {
            let (old_map, new_map) = self.load_highlight_maps(file, old_path, new_path);
            if !old_key.is_empty() && !has_old {
                self.syntax_cache_old.insert(old_key.clone(), old_map);
            }
            if !new_key.is_empty() && !has_new {
                self.syntax_cache_new.insert(new_key.clone(), new_map);
            }
        }

        let old_map = if old_key.is_empty() {
            Vec::new()
        } else {
            self.syntax_cache_old
                .get(&old_key)
                .cloned()
                .unwrap_or_default()
        };
        let new_map = if new_key.is_empty() {
            Vec::new()
        } else {
            self.syntax_cache_new
                .get(&new_key)
                .cloned()
                .unwrap_or_default()
        };

        (old_map, new_map)
    }

    fn load_highlight_maps(
        &mut self,
        file: &DiffFile,
        old_path: Option<&str>,
        new_path: Option<&str>,
    ) -> (Vec<Vec<HighlightRange>>, Vec<Vec<HighlightRange>>) {
        if !self.config.syntax_highlighting {
            return (Vec::new(), Vec::new());
        }

        let mut need_old = matches!(file.status, FileStatus::Deleted);
        let mut need_new = matches!(file.status, FileStatus::Added);
        if !need_old || !need_new {
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
    fn build_diff_hunk_lines(&mut self, file_idx: usize, file: &DiffFile, file_path: &str) {
        let old_path = file
            .old_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let new_path = file
            .new_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let allow_compute = self.eager_highlight_files.contains(file_path)
            || self.syntax_cache_new.contains_key(file_path)
            || self.syntax_cache_old.contains_key(file_path);
        let (old_map, new_map) = self.cached_highlight_maps(
            file,
            old_path.as_deref(),
            new_path.as_deref().or(old_path.as_deref()),
            allow_compute,
        );

        for (hunk_idx, hunk) in file.hunks.iter().enumerate() {
            // Count additions and deletions in this hunk
            let additions = hunk.lines.iter().filter(|l| l.kind == LineKind::Addition).count();
            let deletions = hunk.lines.iter().filter(|l| l.kind == LineKind::Deletion).count();

            // Add spacing before hunk
            self.display_lines.push(DisplayLine::Spacer);

            if let Some(text) = hunk.header.as_ref().filter(|t| !t.trim().is_empty()) {
                let clean = text.trim().trim_start_matches("@@").trim().to_string();
                let highlights = if allow_compute && self.config.syntax_highlighting {
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
                self.display_lines.push(DisplayLine::HunkContext {
                    text: clean,
                    line_no: hunk.new_start,
                    highlights,
                });
            }

            // Add hunk header
            let line_no = hunk.new_start;
            self.display_lines.push(DisplayLine::HunkHeader {
                line_no,
                additions,
                deletions,
                file_idx,
                hunk_idx,
            });

            for line in &hunk.lines {
                let mut highlighted_line = line.clone();
                self.highlight_from_maps(&mut highlighted_line, &old_map, &new_map);

                self.display_lines.push(DisplayLine::Diff {
                    line: highlighted_line.clone(),
                    file_idx,
                    file_path: file_path.to_string(),
                    hunk_idx: Some(hunk_idx),
                });

                self.add_annotations_for_line(file_idx, file_path, &highlighted_line);
            }

            // Add hunk end marker
            self.display_lines.push(DisplayLine::HunkEnd { file_idx, hunk_idx });
        }
    }

    /// Build display lines for full file view (expanded mode)
    fn build_expanded_file_lines(&mut self, file_idx: usize, file: &DiffFile, file_path: &str) {
        use std::collections::HashMap;

        // Build maps of line changes from diff hunks
        // Key: new_line_no for additions/context, old_line_no for deletions
        let mut additions: HashMap<u32, String> = HashMap::new();
        let mut deletions: Vec<(u32, u32, String)> = Vec::new(); // (insert_after_new_line, old_line_no, content)

        let mut hunk_ranges: Vec<(std::ops::RangeInclusive<u32>, std::ops::RangeInclusive<u32>)> =
            Vec::new();
        for hunk in &file.hunks {
            let old_range = hunk.old_start..=hunk.old_start.saturating_add(hunk.old_lines.saturating_sub(1));
            let new_range = hunk.new_start..=hunk.new_start.saturating_add(hunk.new_lines.saturating_sub(1));
            hunk_ranges.push((old_range, new_range));
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
        let (old_map, new_map) = self.cached_highlight_maps(
            file,
            file.old_path.as_ref().map(|p| p.to_string_lossy().to_string()).as_deref(),
            file.new_path.as_ref().map(|p| p.to_string_lossy().to_string()).as_deref(),
            true,
        );

        // Show all lines with changes highlighted
        for (idx, content) in file_lines.iter().enumerate() {
            let line_no = (idx + 1) as u32;

            // First, show any deletions that come before this line
            for (insert_after, old_no, del_content) in &deletions {
                if *insert_after == line_no.saturating_sub(1) {
                    let mut del_line = DiffLine {
                        kind: LineKind::Deletion,
                        old_line_no: Some(*old_no),
                        new_line_no: None,
                        content: del_content.clone(),
                        highlights: Vec::new(),
                    };
                    self.highlight_from_maps(&mut del_line, &old_map, &[]);
                    let hunk_idx = hunk_ranges.iter().position(|(old_range, _)| {
                        old_range.contains(old_no)
                    });
                    self.display_lines.push(DisplayLine::Diff {
                        line: del_line,
                        file_idx,
                        file_path: file_path.to_string(),
                        hunk_idx,
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
            self.display_lines.push(DisplayLine::Diff {
                line: diff_line.clone(),
                file_idx,
                file_path: file_path.to_string(),
                hunk_idx,
            });

            self.add_annotations_for_line(file_idx, file_path, &diff_line);
        }

        // Show any trailing deletions
        let last_line = file_lines.len() as u32;
        for (insert_after, old_no, del_content) in &deletions {
            if *insert_after >= last_line {
                let mut del_line = DiffLine {
                    kind: LineKind::Deletion,
                    old_line_no: Some(*old_no),
                    new_line_no: None,
                    content: del_content.clone(),
                    highlights: Vec::new(),
                };
                self.highlight_from_maps(&mut del_line, &old_map, &[]);
                let hunk_idx = hunk_ranges.iter().position(|(old_range, _)| {
                    old_range.contains(old_no)
                });
                self.display_lines.push(DisplayLine::Diff {
                    line: del_line,
                    file_idx,
                    file_path: file_path.to_string(),
                    hunk_idx,
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
                let orphaned = annotation.side == Side::New
                    && !annotation.anchor_text.is_empty()
                    && annotation.anchor_text.trim() != line.content.trim();
                self.display_lines.push(DisplayLine::Annotation {
                    annotation: annotation.clone(),
                    file_idx,
                    orphaned,
                });
            }
        }
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
                if let Some(lines) = self.read_file_lines(&file_path) {
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
        self.annotation_cursor = 0;
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
        self.annotation_cursor = 0;
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
            KeyCode::Char('?') => {
                self.show_help = !self.show_help;
                if self.show_help {
                    self.help_scroll = 0;
                }
                return Ok(false);
            }
            _ => {}
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
                    self.annotation_cursor = 0;
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
                    self.annotation_cursor = self.annotation_input.chars().count();
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
            self.insert_at_cursor('\n');
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
                self.annotation_cursor = 0;
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
                self.backspace_at_cursor();
            }
            KeyCode::Delete => {
                self.delete_at_cursor();
            }
            KeyCode::Left => {
                self.move_cursor_left();
            }
            KeyCode::Right => {
                self.move_cursor_right();
            }
            KeyCode::Up => {
                self.move_cursor_up();
            }
            KeyCode::Down => {
                self.move_cursor_down();
            }
            KeyCode::Home => {
                self.move_cursor_line_start();
            }
            KeyCode::End => {
                self.move_cursor_line_end();
            }
            KeyCode::Char(c) => {
                self.insert_at_cursor(c);
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
                    self.annotation_input = entry.content.clone();
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
                    self.current_line_idx = idx;
                    self.adjust_scroll();
                    return;
                }
            }
            idx += 1;
        }

        // Wrap to first hunk
        idx = 0;
        while idx < total {
            if let Some(DisplayLine::Diff { hunk_idx: Some(_), .. }) = self.display_lines.get(idx) {
                self.current_line_idx = idx;
                self.adjust_scroll();
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
                    self.current_line_idx = start;
                    self.adjust_scroll();
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
                self.current_line_idx = start;
                self.adjust_scroll();
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

        let Some(hunk) = file.hunks.get(hunk_idx) else {
            self.message = Some("Hunk not found".to_string());
            return Ok(());
        };

        let patch = Self::build_hunk_patch(file, hunk);

        match self.apply_patch_to_index(&patch, reverse) {
            Ok(()) => {
                self.message = Some(if reverse {
                    "Unstaged hunk".to_string()
                } else {
                    "Staged hunk".to_string()
                });
                self.reload_diff()?;
            }
            Err(err) => {
                self.message = Some(format!("Stage/unstage failed: {}", err));
            }
        }

        Ok(())
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
        let files = self.diff_engine.diff(&target, &self.diff_paths)?;

        self.diff_mode = target;
        self.files = files;
        self.display_lines.clear();
        self.current_line_idx = 0;
        self.scroll_offset = 0;
        self.expanded_file = None;
        self.load_collapsed_state();
        self.clear_syntax_cache();
        self.load_all_annotations()?;
        self.prepare_lazy_highlighting();
        self.build_display_lines();

        if self.files.is_empty() {
            let msg = match self.diff_mode {
                DiffMode::Unstaged => "No unstaged changes",
                DiffMode::Staged => "No staged changes",
                _ => "No changes",
            };
            self.message = Some(msg.to_string());
            return Ok(());
        }

        while self.current_line_idx < self.display_lines.len().saturating_sub(1)
            && self.is_skippable_line(self.current_line_idx)
        {
            self.current_line_idx += 1;
        }
        self.adjust_scroll();

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

    fn apply_patch_to_index(&self, patch: &str, reverse: bool) -> Result<(), String> {
        let mut cmd = Command::new("git");
        cmd.arg("apply").arg("--cached");
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
        let files = self.diff_engine.diff(&self.diff_mode, &self.diff_paths)?;
        self.files = files;
        self.display_lines.clear();
        self.current_line_idx = 0;
        self.scroll_offset = 0;
        self.expanded_file = None;
        self.clear_syntax_cache();
        self.load_all_annotations()?;
        self.prepare_lazy_highlighting();
        self.build_display_lines();

        if self.files.is_empty() {
            let msg = match self.diff_mode {
                DiffMode::Unstaged => "No unstaged changes",
                DiffMode::Staged => "No staged changes",
                _ => "No changes",
            };
            self.message = Some(msg.to_string());
            return Ok(());
        }

        if let Some((file_path, line_no)) = target {
            if let Some(idx) = self.find_best_line_match(&file_path, line_no) {
                self.current_line_idx = idx;
            }
        } else {
            while self.current_line_idx < self.display_lines.len().saturating_sub(1)
                && self.is_skippable_line(self.current_line_idx)
            {
                self.current_line_idx += 1;
            }
        }
        self.adjust_scroll();

        Ok(())
    }

    fn clear_syntax_cache(&mut self) {
        self.syntax_cache_old.clear();
        self.syntax_cache_new.clear();
        self.highlight_pending.clear();
        self.highlight_generation = self.highlight_generation.wrapping_add(1);
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

    fn render_input_with_cursor(&self) -> String {
        let mut chars: Vec<char> = self.annotation_input.chars().collect();
        let cursor = self.annotation_cursor.min(chars.len());
        chars.insert(cursor, '|');
        chars.into_iter().collect()
    }

    fn insert_at_cursor(&mut self, ch: char) {
        let mut chars: Vec<char> = self.annotation_input.chars().collect();
        let cursor = self.annotation_cursor.min(chars.len());
        chars.insert(cursor, ch);
        self.annotation_cursor = cursor + 1;
        self.annotation_input = chars.into_iter().collect();
    }

    fn backspace_at_cursor(&mut self) {
        let mut chars: Vec<char> = self.annotation_input.chars().collect();
        if self.annotation_cursor == 0 || chars.is_empty() {
            return;
        }
        let idx = self.annotation_cursor.min(chars.len()) - 1;
        chars.remove(idx);
        self.annotation_cursor = idx;
        self.annotation_input = chars.into_iter().collect();
    }

    fn delete_at_cursor(&mut self) {
        let mut chars: Vec<char> = self.annotation_input.chars().collect();
        if chars.is_empty() {
            return;
        }
        let idx = self.annotation_cursor.min(chars.len());
        if idx >= chars.len() {
            return;
        }
        chars.remove(idx);
        self.annotation_input = chars.into_iter().collect();
    }

    fn move_cursor_left(&mut self) {
        if self.annotation_cursor > 0 {
            self.annotation_cursor -= 1;
        }
    }

    fn move_cursor_right(&mut self) {
        let len = self.annotation_input.chars().count();
        if self.annotation_cursor < len {
            self.annotation_cursor += 1;
        }
    }

    fn move_cursor_line_start(&mut self) {
        let (line_start, _) = self.current_line_bounds();
        self.annotation_cursor = line_start;
    }

    fn move_cursor_line_end(&mut self) {
        let (_, line_end) = self.current_line_bounds();
        self.annotation_cursor = line_end;
    }

    fn move_cursor_up(&mut self) {
        let (line_start, _line_end, col) = self.current_line_bounds_with_col();
        if line_start == 0 {
            return;
        }
        let prev_end = line_start.saturating_sub(1);
        let prev_start = self.line_start_at(prev_end);
        let prev_len = prev_end.saturating_sub(prev_start);
        self.annotation_cursor = prev_start + col.min(prev_len);
    }

    fn move_cursor_down(&mut self) {
        let (_line_start, line_end, col) = self.current_line_bounds_with_col();
        let len = self.annotation_input.chars().count();
        if line_end >= len {
            return;
        }
        let next_start = line_end + 1;
        let next_end = self.line_end_at(next_start);
        let next_len = next_end.saturating_sub(next_start);
        self.annotation_cursor = next_start + col.min(next_len);
    }

    fn current_line_bounds(&self) -> (usize, usize) {
        let (start, end, _) = self.current_line_bounds_with_col();
        (start, end)
    }

    fn current_line_bounds_with_col(&self) -> (usize, usize, usize) {
        let chars: Vec<char> = self.annotation_input.chars().collect();
        let len = chars.len();
        let cursor = self.annotation_cursor.min(len);
        let line_start = self.line_start_at(cursor);
        let line_end = self.line_end_at(cursor);
        let col = cursor.saturating_sub(line_start);
        (line_start, line_end, col)
    }

    fn line_start_at(&self, idx: usize) -> usize {
        let chars: Vec<char> = self.annotation_input.chars().collect();
        let mut i = idx.min(chars.len());
        while i > 0 {
            if chars[i - 1] == '\n' {
                break;
            }
            i -= 1;
        }
        i
    }

    fn line_end_at(&self, idx: usize) -> usize {
        let chars: Vec<char> = self.annotation_input.chars().collect();
        let mut i = idx.min(chars.len());
        while i < chars.len() {
            if chars[i] == '\n' {
                break;
            }
            i += 1;
        }
        i
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
    diff_mode: DiffMode,
    diff_paths: Vec<String>,
) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
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
    app.load_all_annotations()?;
    app.prepare_lazy_highlighting();
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

        while let Ok(evt) = app.ai_rx.try_recv() {
            app.handle_ai_event(evt)?;
        }
        while let Ok(evt) = app.highlight_rx.try_recv() {
            app.handle_highlight_event(evt)?;
        }

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if app.handle_input(key)? {
                    return Ok(());
                }
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
                    let content_spans =
                        build_highlighted_spans(text, highlights, style, false, theme.hunk_ctx_bg);
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

                    // Check for annotation
                    let has_annotation = app.all_annotations.iter().any(|a| {
                        a.file_path == *file_path
                            && a.side == side
                            && a.start_line <= line_no
                            && a.end_line.map_or(a.start_line == line_no, |e| e >= line_no)
                    });
                    let annotation_marker = if has_annotation { "●" } else { " " };

                    let line_num_fg = match line.kind {
                        LineKind::Addition => theme.added_fg,
                        LineKind::Deletion => theme.deleted_fg,
                        LineKind::Context => theme.line_num,
                    };
                    let line_num_style = if is_current {
                        Style::default().fg(line_num_fg).bg(theme.current_line_bg)
                    } else {
                        Style::default().fg(line_num_fg)
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

                    let prefix_spans = vec![
                        Span::styled(annotation_marker, marker_style),
                        Span::styled(line_num, line_num_style),
                        Span::raw(" "),
                        Span::styled(format!("{} ", prefix), content_style),
                    ];

                    let content_spans = build_highlighted_spans(
                        &line.content,
                        &line.highlights,
                        content_style,
                        is_current,
                        theme.current_line_bg,
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

                let collapse_indicator = if app.collapsed_files.contains(path) { " ▶" } else { "" };

                let header = Line::from(vec![
                    Span::styled(format!(" Δ {}{} ", path, collapse_indicator), style),
                    Span::styled(format!("+{} ", adds), Style::default().fg(theme.added_fg).bg(stats_bg)),
                    Span::styled(format!("-{} ", dels), Style::default().fg(theme.deleted_fg).bg(stats_bg)),
                ]);
                left_items.push(ListItem::new(header));
                right_items.push(ListItem::new(Line::from("")));
            }
            DisplayLine::HunkContext { text, line_no, highlights } => {
                let style = Style::default()
                    .fg(theme.hunk_ctx_fg)
                    .bg(theme.hunk_ctx_bg);
                let content_spans =
                    build_highlighted_spans(text, highlights, style, false, theme.hunk_ctx_bg);
                let line_num = format!("{:>4}", line_no);
                let line_num_style = Style::default()
                    .fg(theme.hunk_ctx_fg)
                    .bg(theme.hunk_ctx_bg)
                    .add_modifier(Modifier::BOLD);
                left_items.push(ListItem::new(build_context_box(
                    vec![Span::styled(line_num, line_num_style)],
                    content_spans,
                    columns[0].width as usize,
                    style,
                )));
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
                let line = vec![
                    Span::styled(format!(" +{} -{} ", additions, deletions), stats_style),
                    Span::styled(format!("starting at line {} ", line_no), style),
                ];
                left_items.push(ListItem::new(Line::from(line)));
                right_items.push(ListItem::new(Line::from("")));
            }
            DisplayLine::HunkEnd { .. } => {
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
                        let old_no = line
                            .old_line_no
                            .map_or("    ".to_string(), |n| format!("{:>4}", n));
                        let content_style = Style::default().fg(theme.deleted_fg).bg(theme.deleted_bg);
                        let line_num_style = if is_current {
                            Style::default().fg(theme.deleted_fg).bg(theme.current_line_bg)
                        } else {
                            Style::default().fg(theme.deleted_fg)
                        };
                        let left_spans = vec![
                            Span::styled(marker, marker_style),
                            Span::styled(old_no, line_num_style),
                            Span::styled("- ", content_style),
                        ];
                        let content_spans = build_highlighted_spans(
                            &line.content,
                            &line.highlights,
                            content_style,
                            is_current,
                            theme.current_line_bg,
                        );
                        let lines = wrap_spans_with_prefix(left_spans, content_spans, columns[0].width as usize, content_style);
                        left_items.push(ListItem::new(lines));
                        right_items.push(ListItem::new(Line::from("")));
                    }
                    LineKind::Addition => {
                        let new_no = line
                            .new_line_no
                            .map_or("    ".to_string(), |n| format!("{:>4}", n));
                        let content_style = Style::default().fg(theme.added_fg).bg(theme.added_bg);
                        let line_num_style = if is_current {
                            Style::default().fg(theme.added_fg).bg(theme.current_line_bg)
                        } else {
                            Style::default().fg(theme.added_fg)
                        };
                        left_items.push(ListItem::new(Line::from("")));
                        let right_spans = vec![
                            Span::styled(marker, marker_style),
                            Span::styled(new_no, line_num_style),
                            Span::styled("+ ", content_style),
                        ];
                        let content_spans = build_highlighted_spans(
                            &line.content,
                            &line.highlights,
                            content_style,
                            is_current,
                            theme.current_line_bg,
                        );
                        let lines = wrap_spans_with_prefix(right_spans, content_spans, columns[2].width as usize, content_style);
                        right_items.push(ListItem::new(lines));
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
                        } else {
                            Style::default().fg(theme.line_num)
                        };
                        let left_spans = vec![
                            Span::styled(marker, marker_style),
                            Span::styled(old_no, line_num_style),
                            Span::styled("  ", content_style),
                        ];
                        let content_spans = build_highlighted_spans(
                            &line.content,
                            &line.highlights,
                            content_style,
                            is_current,
                            theme.current_line_bg,
                        );
                        let lines = wrap_spans_with_prefix(left_spans, content_spans, columns[0].width as usize, content_style);
                        left_items.push(ListItem::new(lines));

                        let right_spans = vec![
                            Span::styled(marker, marker_style),
                            Span::styled(new_no, line_num_style),
                            Span::styled("  ", content_style),
                        ];
                        let content_spans = build_highlighted_spans(
                            &line.content,
                            &line.highlights,
                            content_style,
                            is_current,
                            theme.current_line_bg,
                        );
                        let lines = wrap_spans_with_prefix(right_spans, content_spans, columns[2].width as usize, content_style);
                        right_items.push(ListItem::new(lines));
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
                            let wrapped = wrap_spans_with_prefix(
                                prefix_spans,
                                content_spans,
                                columns[0].width as usize,
                                style,
                            );
                            lines.extend(wrapped);
                        }
                        left_items.push(ListItem::new(lines));
                        right_items.push(ListItem::new(Line::from("")));
                    }
                    Side::New => {
                        left_items.push(ListItem::new(Line::from("")));
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
                            let wrapped = wrap_spans_with_prefix(
                                prefix_spans,
                                content_spans,
                                columns[2].width as usize,
                                style,
                            );
                            right_lines.extend(wrapped);
                        }
                        right_items.push(ListItem::new(right_lines));
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
            let shortcuts = " j/k:nav  n/N:chunk  c:collapse  a:add  e:edit  r:resolve";
            let content = if let Some(msg) = &app.message {
                format!("{}{}{}", mode_label, msg, ai_suffix)
            } else {
                format!("{}{}{}", mode_label, shortcuts, ai_suffix)
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
        Mode::AnnotationList => {
            let status = Paragraph::new(" Annotations: j/k, Enter: jump, e: edit, d: delete, Esc: close")
                .style(Style::default().fg(theme.status_fg).bg(theme.status_bg));
            f.render_widget(status, area);
        }
    }
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

fn git_merge_base_at(repo_path: &PathBuf, from: &str, to: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("merge-base")
        .arg(from)
        .arg(to)
        .current_dir(repo_path)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
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

fn compute_highlight_maps_for_file(
    highlighter: &mut SyntaxHighlighter,
    repo_path: &PathBuf,
    diff_mode: &DiffMode,
    file: &DiffFile,
    old_path: Option<&str>,
    new_path: Option<&str>,
) -> (Vec<Vec<HighlightRange>>, Vec<Vec<HighlightRange>>) {
    let mut need_old = matches!(file.status, FileStatus::Deleted);
    let mut need_new = matches!(file.status, FileStatus::Added);
    if !need_old || !need_new {
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
    match diff_mode {
        DiffMode::Unstaged => {
            if need_old {
                if let Some(path) = old_path {
                    old_content = git_show_at(repo_path, &format!(":{}", path));
                }
            }
            if need_new {
                if let Some(path) = new_path {
                    new_content = read_working_file_at(repo_path, path);
                }
            }
        }
        DiffMode::Staged => {
            if need_old {
                if let Some(path) = old_path {
                    old_content = git_show_at(repo_path, &format!("HEAD:{}", path));
                }
            }
            if need_new {
                if let Some(path) = new_path {
                    new_content = git_show_at(repo_path, &format!(":{}", path));
                }
            }
        }
        DiffMode::WorkingTree { base } => {
            if need_old {
                if let Some(path) = old_path {
                    old_content = git_show_at(repo_path, &format!("{}:{}", base, path));
                }
            }
            if need_new {
                if let Some(path) = new_path {
                    new_content = read_working_file_at(repo_path, path);
                }
            }
        }
        DiffMode::Commits { from, to } => {
            if need_old {
                if let Some(path) = old_path {
                    old_content = git_show_at(repo_path, &format!("{}:{}", from, path));
                }
            }
            if need_new {
                if let Some(path) = new_path {
                    new_content = git_show_at(repo_path, &format!("{}:{}", to, path));
                }
            }
        }
        DiffMode::MergeBase { from, to } => {
            let base = git_merge_base_at(repo_path, from, to);
            if need_old {
                if let (Some(path), Some(base)) = (old_path, base.as_deref()) {
                    old_content = git_show_at(repo_path, &format!("{}:{}", base, path));
                }
            }
            if need_new {
                if let Some(path) = new_path {
                    new_content = git_show_at(repo_path, &format!("{}:{}", to, path));
                }
            }
        }
        _ => {
            if need_new {
                if let Some(path) = new_path.or(old_path) {
                    new_content = read_working_file_at(repo_path, path);
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
        .map(|c| build_highlight_map_with(highlighter, c, old_path.unwrap_or("")))
        .unwrap_or_default();
    let new_map = new_content
        .as_ref()
        .map(|c| build_highlight_map_with(highlighter, c, new_path.or(old_path).unwrap_or("")))
        .unwrap_or_default();

    (old_map, new_map)
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
    base_style: Style,
    is_current: bool,
    current_line_bg: Color,
) -> Vec<Span<'static>> {
    let base_style = if is_current {
        base_style
            .bg(current_line_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        base_style
    };

    if highlights.is_empty() {
        return vec![Span::styled(content.to_string(), base_style)];
    }

    let content_bytes = content.as_bytes();
    let mut pos = 0;
    let mut spans: Vec<Span<'static>> = Vec::new();

    let mut sorted: Vec<HighlightRange> = highlights.to_vec();
    sorted.sort_by_key(|h| h.start);
    for highlight in sorted {
        // Ensure we don't go out of bounds
        let mut start = highlight.start.min(content_bytes.len());
        let mut end = highlight.end.min(content_bytes.len());

        if start < pos {
            start = pos;
        }
        if end < pos {
            end = pos;
        }
        if start == end {
            continue;
        }
        if end <= start {
            continue;
        }

        if start > pos {
            // Add unhighlighted segment
            if let Ok(segment) = std::str::from_utf8(&content_bytes[pos..start]) {
                spans.push(Span::styled(segment.to_string(), base_style));
            }
        }

        if end > start {
            // Add highlighted segment
            if let Ok(segment) = std::str::from_utf8(&content_bytes[start..end]) {
                let styled = apply_text_style(base_style, highlight.style, is_current);
                spans.push(Span::styled(segment.to_string(), styled));
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
