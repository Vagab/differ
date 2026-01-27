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
    anchor_line INTEGER DEFAULT 0,
    anchor_text TEXT DEFAULT '',
    context_before TEXT DEFAULT '',
    context_after TEXT DEFAULT '',
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
}

impl AnnotationType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Comment => "comment",
            Self::Todo => "todo",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "comment" => Some(Self::Comment),
            "todo" => Some(Self::Todo),
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
#[allow(dead_code)]
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
    pub anchor_line: u32,
    pub anchor_text: String,
    pub context_before: String,
    pub context_after: String,
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

        // Migrate missing columns
        Self::ensure_annotation_columns(&conn)?;

        Ok(Self { conn })
    }

    fn ensure_annotation_columns(conn: &Connection) -> Result<()> {
        let mut stmt = conn.prepare("PRAGMA table_info(annotations)")?;
        let cols = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<String>>>()?;

        let has_anchor_line = cols.iter().any(|c| c == "anchor_line");
        let has_anchor_text = cols.iter().any(|c| c == "anchor_text");
        let has_context_before = cols.iter().any(|c| c == "context_before");
        let has_context_after = cols.iter().any(|c| c == "context_after");

        if !has_anchor_line {
            conn.execute("ALTER TABLE annotations ADD COLUMN anchor_line INTEGER DEFAULT 0", [])?;
        }
        if !has_anchor_text {
            conn.execute("ALTER TABLE annotations ADD COLUMN anchor_text TEXT DEFAULT ''", [])?;
        }
        if !has_context_before {
            conn.execute("ALTER TABLE annotations ADD COLUMN context_before TEXT DEFAULT ''", [])?;
        }
        if !has_context_after {
            conn.execute("ALTER TABLE annotations ADD COLUMN context_after TEXT DEFAULT ''", [])?;
        }

        Ok(())
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
        anchor_line: u32,
        anchor_text: &str,
        context_before: &str,
        context_after: &str,
    ) -> Result<i64> {
        self.conn.execute(
            r#"
            INSERT INTO annotations (
                repo_id, file_path, commit_sha, side, start_line, end_line,
                annotation_type, content, anchor_line, anchor_text, context_before, context_after
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
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
                anchor_line,
                anchor_text,
                context_before,
                context_after,
            ],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Updates an existing annotation's content and type
    pub fn update_annotation(&self, id: i64, content: &str, annotation_type: AnnotationType) -> Result<()> {
        self.conn.execute(
            "UPDATE annotations SET content = ?1, annotation_type = ?2 WHERE id = ?3",
            params![content, annotation_type.as_str(), id],
        )?;
        Ok(())
    }

    /// Updates an annotation's position and anchor data
    pub fn update_annotation_location(
        &self,
        id: i64,
        start_line: u32,
        end_line: Option<u32>,
        anchor_line: u32,
        anchor_text: &str,
        context_before: &str,
        context_after: &str,
    ) -> Result<()> {
        self.conn.execute(
            r#"
            UPDATE annotations
            SET start_line = ?1, end_line = ?2, anchor_line = ?3, anchor_text = ?4,
                context_before = ?5, context_after = ?6
            WHERE id = ?7
            "#,
            params![
                start_line,
                end_line,
                anchor_line,
                anchor_text,
                context_before,
                context_after,
                id,
            ],
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
    ) -> Result<Vec<Annotation>> {
        let mut sql = String::from(
            r#"
            SELECT id, repo_id, file_path, commit_sha, side, start_line, end_line,
                   annotation_type, content, anchor_line, anchor_text, context_before, context_after,
                   created_at, resolved_at
            FROM annotations
            WHERE repo_id = ?1
            "#,
        );

        if file_path.is_some() {
            sql.push_str(" AND file_path = ?2");
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
    #[allow(dead_code)]
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
                   annotation_type, content, anchor_line, anchor_text, context_before, context_after,
                   created_at, resolved_at
            FROM annotations
            WHERE repo_id = ?1
              AND file_path = ?2
              AND side = ?3
              AND start_line <= ?4
              AND (end_line IS NULL AND start_line = ?4 OR end_line >= ?4)
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

    /// Clears all annotations for a repo
    pub fn clear_all(&self, repo_id: i64) -> Result<usize> {
        let count = self.conn.execute(
            "DELETE FROM annotations WHERE repo_id = ?1",
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
            anchor_line: row.get(9)?,
            anchor_text: row.get(10)?,
            context_before: row.get(11)?,
            context_after: row.get(12)?,
            created_at: row.get(13)?,
            resolved_at: row.get(14)?,
        })
    }

    /// Mark an annotation as resolved
    pub fn resolve_annotation(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE annotations SET resolved_at = CURRENT_TIMESTAMP WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Mark an annotation as unresolved
    pub fn unresolve_annotation(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE annotations SET resolved_at = NULL WHERE id = ?1",
            params![id],
        )?;
        Ok(())
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
                10,
                "This needs refactoring",
                "",
                "",
            )
            .unwrap();

        let annotations = storage.list_annotations(repo_id, None).unwrap();
        assert_eq!(annotations.len(), 1);
        assert_eq!(annotations[0].id, id);
        assert_eq!(annotations[0].content, "This needs refactoring");
    }

    #[test]
    fn test_delete_and_clear() {
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
                1,
                "Fix this",
                "",
                "",
            )
            .unwrap();

        // Should be visible
        let annotations = storage.list_annotations(repo_id, None).unwrap();
        assert_eq!(annotations.len(), 1);

        // Delete it
        storage.delete_annotation(id).unwrap();

        // Should be gone
        let annotations = storage.list_annotations(repo_id, None).unwrap();
        assert_eq!(annotations.len(), 0);
    }
}
