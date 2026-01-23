//! TUI layer using ratatui and crossterm
//!
//! Provides interactive diff viewing with annotation support.

use crate::diff::{DiffEngine, DiffFile, DiffLine, LineKind};
use crate::storage::{Annotation, AnnotationType, Side, Storage};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent},
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

/// Application state
pub struct App {
    storage: Storage,
    diff_engine: DiffEngine,
    repo_path: PathBuf,
    repo_id: i64,

    // Diff state
    files: Vec<DiffFile>,
    current_file_idx: usize,
    current_line_idx: usize,
    scroll_offset: usize,

    // UI state
    mode: Mode,
    annotation_input: String,
    annotation_type: AnnotationType,
    message: Option<String>,
    show_help: bool,

    // Annotations for current file
    file_annotations: Vec<Annotation>,
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
    ) -> Self {
        Self {
            storage,
            diff_engine,
            repo_path,
            repo_id,
            files,
            current_file_idx: 0,
            current_line_idx: 0,
            scroll_offset: 0,
            mode: Mode::Normal,
            annotation_input: String::new(),
            annotation_type: AnnotationType::Comment,
            message: None,
            show_help: false,
            file_annotations: Vec::new(),
        }
    }

    fn current_file(&self) -> Option<&DiffFile> {
        self.files.get(self.current_file_idx)
    }

    fn all_lines(&self) -> Vec<&DiffLine> {
        self.current_file()
            .map(|f| f.hunks.iter().flat_map(|h| h.lines.iter()).collect())
            .unwrap_or_default()
    }

    fn current_line(&self) -> Option<&DiffLine> {
        self.all_lines().get(self.current_line_idx).copied()
    }

    fn load_annotations(&mut self) -> Result<()> {
        if let Some(file) = self.current_file() {
            let path = file
                .new_path
                .as_ref()
                .or(file.old_path.as_ref())
                .map(|p| p.to_string_lossy().to_string());

            if let Some(path) = path {
                self.file_annotations = self
                    .storage
                    .list_annotations(self.repo_id, Some(&path), false)?;
            } else {
                self.file_annotations.clear();
            }
        }
        Ok(())
    }

    fn get_annotation_for_line(&self, line_no: u32, side: Side) -> Option<&Annotation> {
        self.file_annotations.iter().find(|a| {
            a.side == side
                && a.start_line <= line_no
                && a.end_line.map_or(a.start_line == line_no, |e| e >= line_no)
        })
    }

    fn add_annotation(&mut self) -> Result<()> {
        if let Some(line) = self.current_line() {
            let line_no = line.new_line_no.or(line.old_line_no).unwrap_or(1);
            let side = if line.new_line_no.is_some() {
                Side::New
            } else {
                Side::Old
            };

            if let Some(file) = self.current_file() {
                let path = file
                    .new_path
                    .as_ref()
                    .or(file.old_path.as_ref())
                    .map(|p| p.to_string_lossy().to_string());

                if let Some(path) = path {
                    self.storage.add_annotation(
                        self.repo_id,
                        &path,
                        None, // commit_sha - could add later
                        side,
                        line_no,
                        None, // end_line - single line for now
                        self.annotation_type.clone(),
                        &self.annotation_input,
                    )?;

                    self.message = Some("Annotation added".to_string());
                    self.load_annotations()?;
                }
            }
        }

        self.annotation_input.clear();
        self.mode = Mode::Normal;
        Ok(())
    }

    fn edit_annotation(&mut self, id: i64) -> Result<()> {
        self.storage.update_annotation(id, &self.annotation_input)?;
        self.message = Some("Annotation updated".to_string());
        self.load_annotations()?;
        self.annotation_input.clear();
        self.mode = Mode::Normal;
        Ok(())
    }

    fn delete_annotation_at_line(&mut self) -> Result<()> {
        if let Some(line) = self.current_line() {
            let line_no = line.new_line_no.or(line.old_line_no).unwrap_or(1);
            let side = if line.new_line_no.is_some() {
                Side::New
            } else {
                Side::Old
            };

            if let Some(annotation) = self.get_annotation_for_line(line_no, side) {
                let id = annotation.id;
                self.storage.delete_annotation(id)?;
                self.message = Some("Annotation deleted".to_string());
                self.load_annotations()?;
            }
        }
        Ok(())
    }

    fn resolve_annotation_at_line(&mut self) -> Result<()> {
        if let Some(line) = self.current_line() {
            let line_no = line.new_line_no.or(line.old_line_no).unwrap_or(1);
            let side = if line.new_line_no.is_some() {
                Side::New
            } else {
                Side::Old
            };

            if let Some(annotation) = self.get_annotation_for_line(line_no, side) {
                let id = annotation.id;
                self.storage.resolve_annotation(id)?;
                self.message = Some("Annotation resolved".to_string());
                self.load_annotations()?;
            }
        }
        Ok(())
    }

    fn navigate_up(&mut self) {
        if self.current_line_idx > 0 {
            self.current_line_idx -= 1;
            // Adjust scroll
            if self.current_line_idx < self.scroll_offset {
                self.scroll_offset = self.current_line_idx;
            }
        }
    }

    fn navigate_down(&mut self) {
        let total_lines = self.all_lines().len();
        if self.current_line_idx < total_lines.saturating_sub(1) {
            self.current_line_idx += 1;
        }
    }

    fn next_file(&mut self) {
        if self.current_file_idx < self.files.len().saturating_sub(1) {
            self.current_file_idx += 1;
            self.current_line_idx = 0;
            self.scroll_offset = 0;
            let _ = self.load_annotations();
        }
    }

    fn prev_file(&mut self) {
        if self.current_file_idx > 0 {
            self.current_file_idx -= 1;
            self.current_line_idx = 0;
            self.scroll_offset = 0;
            let _ = self.load_annotations();
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
        match key.code {
            KeyCode::Char('q') => return Ok(true), // Quit
            KeyCode::Char('?') => self.show_help = !self.show_help,

            // Navigation
            KeyCode::Char('j') | KeyCode::Down => self.navigate_down(),
            KeyCode::Char('k') | KeyCode::Up => self.navigate_up(),
            KeyCode::Char('n') => self.next_file(),
            KeyCode::Char('N') => self.prev_file(),
            KeyCode::Char('g') => {
                self.current_line_idx = 0;
                self.scroll_offset = 0;
            }
            KeyCode::Char('G') => {
                let total = self.all_lines().len();
                self.current_line_idx = total.saturating_sub(1);
            }

            // Annotations
            KeyCode::Char('a') => {
                self.mode = Mode::AddAnnotation;
                self.annotation_input.clear();
            }
            KeyCode::Char('e') => {
                if let Some(line) = self.current_line() {
                    let line_no = line.new_line_no.or(line.old_line_no).unwrap_or(1);
                    let side = if line.new_line_no.is_some() {
                        Side::New
                    } else {
                        Side::Old
                    };

                    // Extract values before mutating self
                    let annotation_data = self
                        .get_annotation_for_line(line_no, side)
                        .map(|a| (a.id, a.content.clone()));

                    if let Some((id, content)) = annotation_data {
                        self.annotation_input = content;
                        self.mode = Mode::EditAnnotation(id);
                    }
                }
            }
            KeyCode::Char('d') => self.delete_annotation_at_line()?,
            KeyCode::Char('r') => self.resolve_annotation_at_line()?,

            // Annotation type toggle
            KeyCode::Char('t') => {
                self.annotation_type = match self.annotation_type {
                    AnnotationType::Comment => AnnotationType::Todo,
                    AnnotationType::Todo => AnnotationType::AiPrompt,
                    AnnotationType::AiPrompt => AnnotationType::Comment,
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
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.annotation_input.clear();
            }
            KeyCode::Enter => {
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
}

/// Runs the TUI application
pub fn run(
    storage: Storage,
    diff_engine: DiffEngine,
    repo_path: PathBuf,
    repo_id: i64,
    files: Vec<DiffFile>,
) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(storage, diff_engine, repo_path, repo_id, files);
    app.load_annotations()?;

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

fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(0),    // Diff content
            Constraint::Length(3), // Status/input
        ])
        .split(f.area());

    // Header
    render_header(f, app, chunks[0]);

    // Diff content
    render_diff(f, app, chunks[1]);

    // Status bar / input
    render_status(f, app, chunks[2]);

    // Help overlay
    if app.show_help {
        render_help(f);
    }
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let file_info = if let Some(file) = app.current_file() {
        let path = file
            .new_path
            .as_ref()
            .or(file.old_path.as_ref())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "<unknown>".to_string());

        format!(
            " {} [{}/{}]",
            path,
            app.current_file_idx + 1,
            app.files.len()
        )
    } else {
        " No files".to_string()
    };

    let header = Paragraph::new(file_info)
        .style(Style::default().fg(Color::Cyan))
        .block(Block::default().borders(Borders::ALL).title(" differ "));

    f.render_widget(header, area);
}

fn render_diff(f: &mut Frame, app: &App, area: Rect) {
    let lines = app.all_lines();
    let visible_height = area.height as usize - 2; // Account for borders

    // Calculate scroll offset
    let scroll_offset = if app.current_line_idx >= visible_height {
        app.current_line_idx - visible_height + 1
    } else {
        0
    };

    let items: Vec<ListItem> = lines
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_height)
        .map(|(idx, line)| {
            let line_no = line.new_line_no.or(line.old_line_no).unwrap_or(0);
            let side = if line.new_line_no.is_some() {
                Side::New
            } else {
                Side::Old
            };

            let prefix = match line.kind {
                LineKind::Addition => "+",
                LineKind::Deletion => "-",
                LineKind::Context => " ",
            };

            let style = match line.kind {
                LineKind::Addition => Style::default().fg(Color::Green),
                LineKind::Deletion => Style::default().fg(Color::Red),
                LineKind::Context => Style::default(),
            };

            // Check for annotation
            let has_annotation = app.get_annotation_for_line(line_no, side.clone()).is_some();
            let annotation_marker = if has_annotation { "●" } else { " " };

            // Highlight current line
            let style = if idx == app.current_line_idx {
                style.add_modifier(Modifier::REVERSED)
            } else {
                style
            };

            let line_num = format!("{:>4}", line_no);
            let content = format!(
                "{} {} {} {}",
                annotation_marker, line_num, prefix, line.content
            );

            ListItem::new(Line::from(vec![Span::styled(content, style)]))
        })
        .collect();

    let diff_list = List::new(items).block(Block::default().borders(Borders::ALL));

    f.render_widget(diff_list, area);
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let content = match &app.mode {
        Mode::Normal => {
            if let Some(msg) = &app.message {
                msg.clone()
            } else {
                format!(
                    " j/k: navigate | n/N: files | a: add | e: edit | d: delete | r: resolve | t: type ({}) | ?: help | q: quit",
                    app.annotation_type.as_str()
                )
            }
        }
        Mode::AddAnnotation => {
            format!(" Add annotation ({}): {}_", app.annotation_type.as_str(), app.annotation_input)
        }
        Mode::EditAnnotation(_) => {
            format!(" Edit annotation: {}_", app.annotation_input)
        }
    };

    let status = Paragraph::new(content)
        .style(Style::default().fg(Color::Yellow))
        .block(Block::default().borders(Borders::ALL));

    f.render_widget(status, area);
}

fn render_help(f: &mut Frame) {
    let area = centered_rect(60, 70, f.area());

    let help_text = vec![
        "",
        "  Navigation:",
        "    j / ↓     Move down",
        "    k / ↑     Move up",
        "    n         Next file",
        "    N         Previous file",
        "    g         Go to top",
        "    G         Go to bottom",
        "",
        "  Annotations:",
        "    a         Add annotation at current line",
        "    e         Edit annotation at current line",
        "    d         Delete annotation at current line",
        "    r         Mark annotation as resolved",
        "    t         Toggle annotation type",
        "",
        "  Other:",
        "    ?         Toggle this help",
        "    q         Quit",
        "",
        "  In annotation mode:",
        "    Enter     Save annotation",
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
