//! SQLite storage layer for annotations
//!
//! Uses WAL mode for concurrent access and stores repos by hash for privacy.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::Path;

const SCHEMA: &str = r#"
-- Repo lookup table (privacy: store hash, not absolute path)
CREATE TABLE IF NOT EXISTS repos (
    id INTEGER PRIMARY KEY,
    repo_hash TEXT NOT NULL UNIQUE,
    display_name TEXT
);

CREATE TABLE IF NOT EXISTS annotations (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repos(id),
    file_path TEXT NOT NULL,
    commit_sha TEXT,
    side TEXT NOT NULL DEFAULT 'new',
    start_line INTEGER NOT NULL,
    end_line INTEGER,
    annotation_type TEXT NOT NULL DEFAULT 'comment'
        CHECK (annotation_type IN ('comment', 'todo', 'ai_prompt')),
    content TEXT NOT NULL,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    resolved_at DATETIME,

    CHECK (start_line > 0),
    CHECK (end_line IS NULL OR end_line >= start_line)
);

CREATE INDEX IF NOT EXISTS idx_repo_file ON annotations(repo_id, file_path);
CREATE INDEX IF NOT EXISTS idx_unresolved ON annotations(resolved_at) WHERE resolved_at IS NULL;
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnotationType {
    Comment,
    Todo,
    AiPrompt,
}

impl AnnotationType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Comment => "comment",
            Self::Todo => "todo",
            Self::AiPrompt => "ai_prompt",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "comment" => Some(Self::Comment),
            "todo" => Some(Self::Todo),
            "ai_prompt" => Some(Self::AiPrompt),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Side {
    Old,
    New,
}

impl Side {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Old => "old",
            Self::New => "new",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "old" => Some(Self::Old),
            "new" => Some(Self::New),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Annotation {
    pub id: i64,
    pub repo_id: i64,
    pub file_path: String,
    pub commit_sha: Option<String>,
    pub side: Side,
    pub start_line: u32,
    pub end_line: Option<u32>,
    pub annotation_type: AnnotationType,
    pub content: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
}

pub struct Storage {
    conn: Connection,
}

impl Storage {
    /// Opens or creates the database at the default location
    pub fn open_default() -> Result<Self> {
        let config_dir = dirs::config_dir()
            .context("Could not determine config directory")?
            .join("differ");

        std::fs::create_dir_all(&config_dir)
            .context("Failed to create config directory")?;

        let db_path = config_dir.join("annotations.db");
        Self::open(&db_path)
    }

    /// Opens or creates the database at the specified path
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .context("Failed to open database")?;

        // Enable WAL mode for concurrent access
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;

        // Set busy timeout to 30 seconds
        conn.busy_timeout(std::time::Duration::from_secs(30))?;

        // Initialize schema
        conn.execute_batch(SCHEMA)
            .context("Failed to initialize database schema")?;

        Ok(Self { conn })
    }

    /// Generates a hash for a repository path
    pub fn hash_repo_path(path: &Path) -> String {
        let mut hasher = Sha256::new();
        hasher.update(path.to_string_lossy().as_bytes());
        let result = hasher.finalize();
        hex::encode(result)
    }

    /// Gets or creates a repo entry, returns the repo_id
    pub fn get_or_create_repo(&self, repo_path: &Path, display_name: Option<&str>) -> Result<i64> {
        let repo_hash = Self::hash_repo_path(repo_path);

        // Try to find existing repo
        let existing: Option<i64> = self.conn
            .query_row(
                "SELECT id FROM repos WHERE repo_hash = ?1",
                params![repo_hash],
                |row| row.get(0),
            )
            .optional()?;

        if let Some(id) = existing {
            // Update display name if provided
            if let Some(name) = display_name {
                self.conn.execute(
                    "UPDATE repos SET display_name = ?1 WHERE id = ?2",
                    params![name, id],
                )?;
            }
            return Ok(id);
        }

        // Create new repo entry
        self.conn.execute(
            "INSERT INTO repos (repo_hash, display_name) VALUES (?1, ?2)",
            params![repo_hash, display_name],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Adds a new annotation
    pub fn add_annotation(
        &self,
        repo_id: i64,
        file_path: &str,
        commit_sha: Option<&str>,
        side: Side,
        start_line: u32,
        end_line: Option<u32>,
        annotation_type: AnnotationType,
        content: &str,
    ) -> Result<i64> {
        self.conn.execute(
            r#"
            INSERT INTO annotations (
                repo_id, file_path, commit_sha, side, start_line, end_line,
                annotation_type, content
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                repo_id,
                file_path,
                commit_sha,
                side.as_str(),
                start_line,
                end_line,
                annotation_type.as_str(),
                content,
            ],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Updates an existing annotation's content
    pub fn update_annotation(&self, id: i64, content: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE annotations SET content = ?1 WHERE id = ?2",
            params![content, id],
        )?;
        Ok(())
    }

    /// Marks an annotation as resolved
    pub fn resolve_annotation(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE annotations SET resolved_at = CURRENT_TIMESTAMP WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Deletes an annotation
    pub fn delete_annotation(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM annotations WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Lists annotations for a repo, optionally filtered by file
    pub fn list_annotations(
        &self,
        repo_id: i64,
        file_path: Option<&str>,
        include_resolved: bool,
    ) -> Result<Vec<Annotation>> {
        let mut sql = String::from(
            r#"
            SELECT id, repo_id, file_path, commit_sha, side, start_line, end_line,
                   annotation_type, content, created_at, resolved_at
            FROM annotations
            WHERE repo_id = ?1
            "#,
        );

        if let Some(_) = file_path {
            sql.push_str(" AND file_path = ?2");
        }

        if !include_resolved {
            sql.push_str(" AND resolved_at IS NULL");
        }

        sql.push_str(" ORDER BY file_path, start_line");

        let mut stmt = self.conn.prepare(&sql)?;

        let rows = if let Some(fp) = file_path {
            stmt.query_map(params![repo_id, fp], Self::row_to_annotation)?
        } else {
            stmt.query_map(params![repo_id], Self::row_to_annotation)?
        };

        rows.collect::<Result<Vec<_>, _>>()
            .context("Failed to fetch annotations")
    }

    /// Gets annotations for a specific line range in a file
    pub fn get_annotations_for_line(
        &self,
        repo_id: i64,
        file_path: &str,
        line: u32,
        side: Side,
    ) -> Result<Vec<Annotation>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, repo_id, file_path, commit_sha, side, start_line, end_line,
                   annotation_type, content, created_at, resolved_at
            FROM annotations
            WHERE repo_id = ?1
              AND file_path = ?2
              AND side = ?3
              AND start_line <= ?4
              AND (end_line IS NULL AND start_line = ?4 OR end_line >= ?4)
              AND resolved_at IS NULL
            ORDER BY start_line
            "#,
        )?;

        let rows = stmt.query_map(
            params![repo_id, file_path, side.as_str(), line],
            Self::row_to_annotation,
        )?;

        rows.collect::<Result<Vec<_>, _>>()
            .context("Failed to fetch annotations for line")
    }

    /// Clears all resolved annotations for a repo
    pub fn clear_resolved(&self, repo_id: i64) -> Result<usize> {
        let count = self.conn.execute(
            "DELETE FROM annotations WHERE repo_id = ?1 AND resolved_at IS NOT NULL",
            params![repo_id],
        )?;
        Ok(count)
    }

    fn row_to_annotation(row: &rusqlite::Row) -> rusqlite::Result<Annotation> {
        Ok(Annotation {
            id: row.get(0)?,
            repo_id: row.get(1)?,
            file_path: row.get(2)?,
            commit_sha: row.get(3)?,
            side: Side::from_str(row.get::<_, String>(4)?.as_str()).unwrap_or(Side::New),
            start_line: row.get(5)?,
            end_line: row.get(6)?,
            annotation_type: AnnotationType::from_str(row.get::<_, String>(7)?.as_str())
                .unwrap_or(AnnotationType::Comment),
            content: row.get(8)?,
            created_at: row.get(9)?,
            resolved_at: row.get(10)?,
        })
    }
}

// Need hex encoding for the hash
mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().map(|b| format!("{:02x}", b)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_create_and_list_annotations() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::open(&db_path).unwrap();

        let repo_id = storage
            .get_or_create_repo(Path::new("/test/repo"), Some("test-repo"))
            .unwrap();

        let id = storage
            .add_annotation(
                repo_id,
                "src/main.rs",
                Some("abc123"),
                Side::New,
                10,
                Some(15),
                AnnotationType::Comment,
                "This needs refactoring",
            )
            .unwrap();

        let annotations = storage.list_annotations(repo_id, None, false).unwrap();
        assert_eq!(annotations.len(), 1);
        assert_eq!(annotations[0].id, id);
        assert_eq!(annotations[0].content, "This needs refactoring");
    }

    #[test]
    fn test_resolve_and_clear() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::open(&db_path).unwrap();

        let repo_id = storage
            .get_or_create_repo(Path::new("/test/repo"), None)
            .unwrap();

        let id = storage
            .add_annotation(
                repo_id,
                "test.rs",
                None,
                Side::New,
                1,
                None,
                AnnotationType::Todo,
                "Fix this",
            )
            .unwrap();

        // Should be visible before resolving
        let annotations = storage.list_annotations(repo_id, None, false).unwrap();
        assert_eq!(annotations.len(), 1);

        // Resolve it
        storage.resolve_annotation(id).unwrap();

        // Should be hidden by default
        let annotations = storage.list_annotations(repo_id, None, false).unwrap();
        assert_eq!(annotations.len(), 0);

        // Should be visible with include_resolved
        let annotations = storage.list_annotations(repo_id, None, true).unwrap();
        assert_eq!(annotations.len(), 1);

        // Clear resolved
        let cleared = storage.clear_resolved(repo_id).unwrap();
        assert_eq!(cleared, 1);

        // Should be gone completely
        let annotations = storage.list_annotations(repo_id, None, true).unwrap();
        assert_eq!(annotations.len(), 0);
    }
}
