//! Syntax highlighting module using syntect-assets (bat themes)
//!
//! Highlights full files and returns per-line highlight ranges.

use anyhow::Result;
use std::str::FromStr;
use syntect::easy::HighlightLines;
use syntect::highlighting::HighlightIterator;
use syntect::highlighting::{FontStyle, HighlightState, Highlighter, ScopeSelectors};
use syntect::parsing::{ParseState, ScopeStack};
use syntect::util::LinesWithEndings;
use syntect_assets::assets::HighlightingAssets;

/// A simple style used for diff highlighting (foreground + modifiers).
#[derive(Debug, Clone, Copy)]
pub struct TextStyle {
    pub fg: (u8, u8, u8),
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// A highlighted range within a line
#[derive(Debug, Clone)]
pub struct SyntaxHighlight {
    pub start: usize,
    pub end: usize,
    pub style: TextStyle,
}

/// Syntax highlighter using syntect parsers and themes
pub struct SyntaxHighlighter {
    assets: HighlightingAssets,
    theme_name: String,
    string_selector: ScopeSelectors,
}

impl SyntaxHighlighter {
    /// Create a new syntax highlighter with supported language parsers
    pub fn new(theme_name: Option<&str>) -> Result<Self> {
        let assets = HighlightingAssets::from_binary();
        let theme_name = theme_name
            .map(|s| s.to_string())
            .or_else(|| std::env::var("BAT_THEME").ok())
            .unwrap_or_else(|| HighlightingAssets::default_theme().to_string());
        let string_selector = ScopeSelectors::from_str("string")
            .unwrap_or_else(|_| ScopeSelectors::from_str("text").unwrap());
        Ok(Self {
            assets,
            theme_name,
            string_selector,
        })
    }

    /// Highlight an entire file and return per-line highlights
    pub fn highlight_file(&mut self, content: &str, file_path: &str) -> Vec<Vec<SyntaxHighlight>> {
        let theme = self.assets.get_theme(&self.theme_name);
        let Ok(syntax_set) = self.assets.get_syntax_set() else {
            return Vec::new();
        };
        let syntax = syntax_set
            .find_syntax_for_file(file_path)
            .ok()
            .flatten()
            .or_else(|| {
                std::path::Path::new(file_path)
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .and_then(|ext| syntax_set.find_syntax_by_extension(ext))
            })
            .unwrap_or_else(|| syntax_set.find_syntax_plain_text());

        let mut highlighter = HighlightLines::new(syntax, theme);

        let mut per_line: Vec<Vec<SyntaxHighlight>> = Vec::new();
        for line in LinesWithEndings::from(content) {
            let ranges = match highlighter.highlight_line(line, syntax_set) {
                Ok(r) => r,
                Err(_) => {
                    per_line.push(Vec::new());
                    continue;
                }
            };

            let mut line_highlights: Vec<SyntaxHighlight> = Vec::new();
            let mut offset = 0usize;
            for (style, text) in ranges {
                let len = text.len();
                if len == 0 {
                    continue;
                }
                let h = SyntaxHighlight {
                    start: offset,
                    end: offset + len,
                    style: Self::to_text_style(style),
                };
                line_highlights.push(h);
                offset += len;
            }

            per_line.push(line_highlights);
        }

        per_line
    }

    /// Highlight lines and report whether any line ended inside a string scope.
    pub fn highlight_lines_with_string_state(
        &mut self,
        lines: &[&str],
        file_path: &str,
    ) -> (Vec<Vec<SyntaxHighlight>>, bool) {
        let theme = self.assets.get_theme(&self.theme_name);
        let Ok(syntax_set) = self.assets.get_syntax_set() else {
            return (Vec::new(), false);
        };
        let syntax = syntax_set
            .find_syntax_for_file(file_path)
            .ok()
            .flatten()
            .or_else(|| {
                std::path::Path::new(file_path)
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .and_then(|ext| syntax_set.find_syntax_by_extension(ext))
            })
            .unwrap_or_else(|| syntax_set.find_syntax_plain_text());

        let highlighter = Highlighter::new(theme);
        let mut highlight_state = HighlightState::new(&highlighter, ScopeStack::new());
        let mut parse_state = ParseState::new(syntax);

        let mut per_line: Vec<Vec<SyntaxHighlight>> = Vec::with_capacity(lines.len());
        let mut ends_in_string = false;

        for line in lines {
            let mut text = String::with_capacity(line.len() + 1);
            text.push_str(line);
            text.push('\n');

            let ops = match parse_state.parse_line(&text, syntax_set) {
                Ok(o) => o,
                Err(_) => {
                    per_line.push(Vec::new());
                    continue;
                }
            };

            let iter = HighlightIterator::new(&mut highlight_state, &ops[..], &text, &highlighter);
            let mut line_highlights: Vec<SyntaxHighlight> = Vec::new();
            let mut offset = 0usize;
            for (style, segment) in iter {
                let mut len = segment.len();
                if len == 0 {
                    continue;
                }
                if segment.ends_with('\n') {
                    len = len.saturating_sub(1);
                }
                if len == 0 {
                    continue;
                }
                let h = SyntaxHighlight {
                    start: offset,
                    end: offset + len,
                    style: Self::to_text_style(style),
                };
                line_highlights.push(h);
                offset += len;
            }
            per_line.push(line_highlights);

            if self
                .string_selector
                .does_match(highlight_state.path.as_slice())
                .is_some()
            {
                ends_in_string = true;
            }
        }

        (per_line, ends_in_string)
    }

    fn to_text_style(style: syntect::highlighting::Style) -> TextStyle {
        let fg = (style.foreground.r, style.foreground.g, style.foreground.b);
        TextStyle {
            fg,
            bold: style.font_style.contains(FontStyle::BOLD),
            italic: style.font_style.contains(FontStyle::ITALIC),
            underline: style.font_style.contains(FontStyle::UNDERLINE),
        }
    }
}
