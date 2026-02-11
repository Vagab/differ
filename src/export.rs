//! Export functionality for annotations
//!
//! Exports annotations in markdown format suitable for AI context.

use crate::storage::{Annotation, AnnotationType, Side, Storage};
use anyhow::Result;
use std::collections::BTreeMap;

fn annotation_code_excerpt(annotation: &Annotation) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    if !annotation.context_before.is_empty() {
        lines.extend(
            annotation
                .context_before
                .lines()
                .map(|line| line.to_string()),
        );
    }
    if !annotation.anchor_text.is_empty() {
        lines.push(annotation.anchor_text.clone());
    }
    if !annotation.context_after.is_empty() {
        lines.extend(
            annotation
                .context_after
                .lines()
                .map(|line| line.to_string()),
        );
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn append_markdown_code_context(output: &mut String, annotation: &Annotation) {
    let Some(code_excerpt) = annotation_code_excerpt(annotation) else {
        return;
    };

    let label = match annotation.side {
        Side::Old => "Old code context",
        Side::New => "Code context",
    };

    output.push_str(&format!("**{}:**\n\n", label));
    output.push_str("```text\n");
    output.push_str(&code_excerpt);
    if !code_excerpt.ends_with('\n') {
        output.push('\n');
    }
    output.push_str("```\n\n");
}

/// Exports annotations for a repository in markdown format
pub fn export_markdown(storage: &Storage, repo_id: i64) -> Result<String> {
    let annotations = storage.list_annotations(repo_id, None)?;

    if annotations.is_empty() {
        return Ok("# No annotations found\n".to_string());
    }

    // Group annotations by file
    let mut by_file: BTreeMap<String, Vec<&Annotation>> = BTreeMap::new();
    for annotation in &annotations {
        by_file
            .entry(annotation.file_path.clone())
            .or_default()
            .push(annotation);
    }

    let mut output = String::new();
    output.push_str("# Code Annotations\n\n");

    for (file_path, file_annotations) in by_file {
        output.push_str(&format!("## {}\n\n", file_path));

        for annotation in file_annotations {
            let side_indicator = match annotation.side {
                Side::Old => " (deleted code)",
                Side::New => "",
            };

            let line_range = if let Some(end) = annotation.end_line {
                format!("L{}-{}", annotation.start_line, end)
            } else {
                format!("L{}", annotation.start_line)
            };

            match annotation.annotation_type {
                AnnotationType::Todo => {
                    let mut lines = annotation.content.lines();
                    let first = lines.next().unwrap_or("");
                    output.push_str(&format!(
                        "- [ ] {}{}: {}\n",
                        line_range, side_indicator, first
                    ));
                    for line in lines {
                        output.push_str(&format!("  {}\n", line));
                    }
                    output.push('\n');
                    append_markdown_code_context(&mut output, annotation);
                }
                AnnotationType::Comment => {
                    output.push_str(&format!("### 💬 {}{}\n\n", line_range, side_indicator));
                    output.push_str(&annotation.content);
                    output.push_str("\n\n");
                    append_markdown_code_context(&mut output, annotation);
                }
            }

            if let Some(ref commit) = annotation.commit_sha {
                output.push_str(&format!("_Commit: {}_\n\n", &commit[..7.min(commit.len())]));
            }
        }
    }

    Ok(output)
}

/// Exports annotations as JSON for programmatic consumption
pub fn export_json(storage: &Storage, repo_id: i64) -> Result<String> {
    let annotations = storage.list_annotations(repo_id, None)?;

    #[derive(serde::Serialize)]
    struct ExportAnnotation {
        file_path: String,
        line: u32,
        end_line: Option<u32>,
        side: String,
        annotation_type: String,
        content: String,
        anchor_line: u32,
        anchor_text: String,
        context_before: String,
        context_after: String,
        code_excerpt: Option<String>,
        commit_sha: Option<String>,
    }

    let export: Vec<ExportAnnotation> = annotations
        .into_iter()
        .map(|a| {
            let code_excerpt = annotation_code_excerpt(&a);

            ExportAnnotation {
                file_path: a.file_path,
                line: a.start_line,
                end_line: a.end_line,
                side: a.side.as_str().to_string(),
                annotation_type: a.annotation_type.as_str().to_string(),
                content: a.content,
                anchor_line: a.anchor_line,
                anchor_text: a.anchor_text,
                context_before: a.context_before,
                context_after: a.context_after,
                code_excerpt,
                commit_sha: a.commit_sha,
            }
        })
        .collect();

    serde_json::to_string_pretty(&export).map_err(Into::into)
}

/// Export format options
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Markdown,
    Json,
}

impl ExportFormat {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "md" | "markdown" => Some(Self::Markdown),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// Main export function that handles format selection
pub fn export(storage: &Storage, repo_id: i64, format: ExportFormat) -> Result<String> {
    match format {
        ExportFormat::Markdown => export_markdown(storage, repo_id),
        ExportFormat::Json => export_json(storage, repo_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;
    use std::path::Path;
    use tempfile::tempdir;

    #[test]
    fn test_export_markdown() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::open(&db_path).unwrap();

        let repo_id = storage
            .get_or_create_repo(Path::new("/test/repo"), Some("test"))
            .unwrap();

        storage
            .add_annotation(
                repo_id,
                "src/main.rs",
                None,
                Side::New,
                10,
                None,
                AnnotationType::Todo,
                "Refactor this function",
                10,
                "Refactor this function",
                "",
                "",
            )
            .unwrap();

        storage
            .add_annotation(
                repo_id,
                "src/lib.rs",
                None,
                Side::New,
                5,
                Some(10),
                AnnotationType::Comment,
                "Add error handling here",
                5,
                "Add error handling here",
                "",
                "",
            )
            .unwrap();

        let md = export_markdown(&storage, repo_id).unwrap();

        assert!(md.contains("# Code Annotations"));
        assert!(md.contains("## src/lib.rs"));
        assert!(md.contains("## src/main.rs"));
        assert!(md.contains("Refactor this function"));
        assert!(md.contains("- [ ] L10: Refactor this function"));
        assert!(md.contains("Add error handling here"));
        assert!(md.contains("L5-10"));
        assert!(md.contains("**Code context:**"));
    }

    #[test]
    fn test_export_markdown_old_side_context() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::open(&db_path).unwrap();

        let repo_id = storage
            .get_or_create_repo(Path::new("/test/repo"), Some("test"))
            .unwrap();

        storage
            .add_annotation(
                repo_id,
                "src/legacy.rs",
                None,
                Side::Old,
                42,
                None,
                AnnotationType::Comment,
                "Why was this removed?",
                42,
                "let removed = true;",
                "",
                "",
            )
            .unwrap();

        let md = export_markdown(&storage, repo_id).unwrap();

        assert!(md.contains("L42 (deleted code)"));
        assert!(md.contains("**Old code context:**"));
        assert!(md.contains("let removed = true;"));
    }

    #[test]
    fn test_export_json() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::open(&db_path).unwrap();

        let repo_id = storage
            .get_or_create_repo(Path::new("/test/repo"), None)
            .unwrap();

        storage
            .add_annotation(
                repo_id,
                "test.rs",
                None,
                Side::New,
                1,
                None,
                AnnotationType::Comment,
                "Test annotation",
                1,
                "Test annotation",
                "",
                "",
            )
            .unwrap();

        let json = export_json(&storage, repo_id).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_eq!(parsed[0]["content"], "Test annotation");
        assert_eq!(parsed[0]["anchor_line"], 1);
        assert_eq!(parsed[0]["anchor_text"], "Test annotation");
        assert_eq!(parsed[0]["code_excerpt"], "Test annotation");
    }
}
