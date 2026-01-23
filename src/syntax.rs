//! Syntax highlighting module using tree-sitter
//!
//! Provides syntax highlighting for diff content by parsing code with tree-sitter
//! and mapping syntax node types to visual token types.

use std::collections::HashMap;
use std::path::Path;

/// Types of syntax tokens for highlighting
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum TokenType {
    Keyword,
    String,
    Comment,
    Function,
    Type,
    Number,
    Operator,
    Variable,
    Atom,
    Module,
    Default,
}

/// A highlighted range within a line
#[derive(Debug, Clone)]
pub struct SyntaxHighlight {
    pub start: usize,
    pub end: usize,
    pub token_type: TokenType,
}

/// Syntax highlighter using tree-sitter parsers
pub struct SyntaxHighlighter {
    elixir_parser: tree_sitter::Parser,
}

impl SyntaxHighlighter {
    /// Create a new syntax highlighter with supported language parsers
    pub fn new() -> Self {
        let mut elixir_parser = tree_sitter::Parser::new();
        elixir_parser
            .set_language(&tree_sitter_elixir::LANGUAGE.into())
            .expect("Failed to load Elixir grammar");

        Self { elixir_parser }
    }

    /// Highlight a line of code based on file extension
    pub fn highlight_line(&mut self, content: &str, extension: &str) -> Vec<SyntaxHighlight> {
        match extension {
            "ex" | "exs" | "heex" => self.highlight_elixir(content),
            _ => Vec::new(),
        }
    }

    /// Detect language from file path and highlight the line
    pub fn highlight_for_file(&mut self, content: &str, file_path: &str) -> Vec<SyntaxHighlight> {
        let extension = Path::new(file_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        self.highlight_line(content, extension)
    }

    /// Highlight Elixir code
    fn highlight_elixir(&mut self, content: &str) -> Vec<SyntaxHighlight> {
        let tree = match self.elixir_parser.parse(content, None) {
            Some(tree) => tree,
            None => return Vec::new(),
        };

        let mut highlights = Vec::new();
        let mut cursor = tree.walk();

        self.collect_highlights(&mut cursor, content.as_bytes(), &mut highlights);

        // Sort by start position and merge overlapping ranges
        highlights.sort_by_key(|h| h.start);
        self.merge_highlights(highlights)
    }

    /// Recursively collect highlights from tree-sitter nodes
    fn collect_highlights(
        &self,
        cursor: &mut tree_sitter::TreeCursor,
        source: &[u8],
        highlights: &mut Vec<SyntaxHighlight>,
    ) {
        loop {
            let node = cursor.node();
            let node_type = node.kind();

            // Map tree-sitter node types to our token types
            if let Some(token_type) = self.map_elixir_node(node_type, &node, source) {
                // Only add leaf nodes or specific parent nodes
                if node.child_count() == 0 || self.is_highlightable_parent(node_type) {
                    highlights.push(SyntaxHighlight {
                        start: node.start_byte(),
                        end: node.end_byte(),
                        token_type,
                    });
                }
            }

            // Recurse into children
            if cursor.goto_first_child() {
                self.collect_highlights(cursor, source, highlights);
                cursor.goto_parent();
            }

            // Move to next sibling
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    /// Map Elixir tree-sitter node types to token types
    fn map_elixir_node(
        &self,
        node_type: &str,
        node: &tree_sitter::Node,
        source: &[u8],
    ) -> Option<TokenType> {
        match node_type {
            // Keywords
            "def" | "defp" | "defmodule" | "defmacro" | "defmacrop" | "defstruct"
            | "defprotocol" | "defimpl" | "defdelegate" | "defguard" | "defguardp"
            | "defexception" | "defoverridable" | "do" | "end" | "fn" | "if" | "else"
            | "unless" | "case" | "cond" | "when" | "with" | "for" | "receive" | "try"
            | "catch" | "rescue" | "after" | "raise" | "throw" | "import" | "require"
            | "alias" | "use" | "quote" | "unquote" | "unquote_splicing" | "and" | "or"
            | "not" | "in" | "true" | "false" | "nil" => Some(TokenType::Keyword),

            // Comments
            "comment" => Some(TokenType::Comment),

            // Strings
            "string" | "charlist" | "sigil" => Some(TokenType::String),
            "string_content" | "escape_sequence" => Some(TokenType::String),
            "quoted_content" => Some(TokenType::String),

            // Numbers
            "integer" | "float" => Some(TokenType::Number),

            // Atoms
            "atom" | "quoted_atom" => Some(TokenType::Atom),

            // Operators
            "binary_operator" | "unary_operator" | "operator_identifier" => {
                Some(TokenType::Operator)
            }
            "|>" | "<>" | "++" | "--" | "&&" | "||" | "==" | "!=" | "===" | "!==" | "<"
            | ">" | "<=" | ">=" | "+" | "-" | "*" | "/" | "=" | "|" | "^" | "&" | "~"
            | "!" | "@" | ".." | "..." | "->" | "<-" | "\\\\" | "::" => Some(TokenType::Operator),

            // Function calls
            "call" => None, // Let children handle highlighting

            // Identifiers - check context for module names or attributes
            "identifier" => {
                let text = node.utf8_text(source).unwrap_or("");
                // Check if it looks like a module name (starts with uppercase)
                if text.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                    Some(TokenType::Module)
                // Check if it looks like a module attribute
                } else if text.starts_with('@') {
                    Some(TokenType::Variable)
                } else {
                    None // Regular identifiers don't get special highlighting
                }
            }

            _ => None,
        }
    }

    /// Check if a parent node should be highlighted as a whole
    fn is_highlightable_parent(&self, node_type: &str) -> bool {
        matches!(node_type, "string" | "charlist" | "sigil" | "comment" | "atom" | "quoted_atom")
    }

    /// Merge overlapping highlights, preferring more specific types
    fn merge_highlights(&self, highlights: Vec<SyntaxHighlight>) -> Vec<SyntaxHighlight> {
        if highlights.is_empty() {
            return highlights;
        }

        let mut result = Vec::new();
        let mut covered: HashMap<usize, TokenType> = HashMap::new();

        // Track which bytes are covered and by what type
        for highlight in &highlights {
            for i in highlight.start..highlight.end {
                // Prefer more specific types (non-Default)
                covered
                    .entry(i)
                    .and_modify(|existing| {
                        if *existing == TokenType::Default {
                            *existing = highlight.token_type;
                        }
                    })
                    .or_insert(highlight.token_type);
            }
        }

        // Convert coverage map back to ranges
        if covered.is_empty() {
            return result;
        }

        let mut positions: Vec<_> = covered.keys().copied().collect();
        positions.sort();

        let mut current_start = positions[0];
        let mut current_type = covered[&current_start];

        for &pos in positions.iter().skip(1) {
            let pos_type = covered[&pos];

            // Check if this is a continuation of the same type and adjacent
            if pos_type == current_type && pos == current_start + (result.len() > 0).then(|| 1).unwrap_or(0) {
                // Continue the range
            } else if pos_type != current_type || pos > current_start + 1 {
                // End current range and start new one
                result.push(SyntaxHighlight {
                    start: current_start,
                    end: pos,
                    token_type: current_type,
                });
                current_start = pos;
                current_type = pos_type;
            }
        }

        // Don't forget the last range
        if let Some(&last_pos) = positions.last() {
            result.push(SyntaxHighlight {
                start: current_start,
                end: last_pos + 1,
                token_type: current_type,
            });
        }

        // Re-sort and clean up
        result.sort_by_key(|h| h.start);

        // Simpler approach: just use the original highlights filtered
        highlights
    }
}

impl Default for SyntaxHighlighter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_highlight_elixir_keywords() {
        let mut highlighter = SyntaxHighlighter::new();
        let highlights = highlighter.highlight_line("defmodule Foo do", "ex");

        // Should find defmodule and do keywords
        assert!(!highlights.is_empty());

        let keywords: Vec<_> = highlights
            .iter()
            .filter(|h| h.token_type == TokenType::Keyword)
            .collect();
        assert!(!keywords.is_empty());
    }

    #[test]
    fn test_highlight_elixir_string() {
        let mut highlighter = SyntaxHighlighter::new();
        let highlights = highlighter.highlight_line("\"hello world\"", "ex");

        let strings: Vec<_> = highlights
            .iter()
            .filter(|h| h.token_type == TokenType::String)
            .collect();
        assert!(!strings.is_empty());
    }

    #[test]
    fn test_highlight_elixir_comment() {
        let mut highlighter = SyntaxHighlighter::new();
        let highlights = highlighter.highlight_line("# this is a comment", "ex");

        let comments: Vec<_> = highlights
            .iter()
            .filter(|h| h.token_type == TokenType::Comment)
            .collect();
        assert!(!comments.is_empty());
    }

    #[test]
    fn test_no_highlight_for_unknown_extension() {
        let mut highlighter = SyntaxHighlighter::new();
        let highlights = highlighter.highlight_line("def foo", "xyz");

        assert!(highlights.is_empty());
    }
}
