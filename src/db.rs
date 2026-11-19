//! SQLite storage for direct messages: the conversations between two accounts, their messages, reactions,
//! pins and read positions. It keeps its own file, apart from the chat servers and the auth service.

use rusqlite::{params, Connection, OptionalExtension};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS people (
    id   TEXT PRIMARY KEY,
    name TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS conversations (
    id   INTEGER PRIMARY KEY AUTOINCREMENT,
    a    TEXT NOT NULL,
    b    TEXT NOT NULL,
    UNIQUE (a, b)
);
CREATE TABLE IF NOT EXISTS messages (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation INTEGER NOT NULL,
    author       TEXT NOT NULL,
    text         TEXT NOT NULL,
    timestamp    INTEGER NOT NULL,
    edited       INTEGER NOT NULL DEFAULT 0,
    reply_to     INTEGER,
    pinned       INTEGER NOT NULL DEFAULT 0,
    seq          INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS messages_by_conversation ON messages (conversation, id);
CREATE INDEX IF NOT EXISTS messages_by_seq ON messages (seq);
CREATE TABLE IF NOT EXISTS reactions (
    message_id INTEGER NOT NULL,
    user       TEXT NOT NULL,
    emoji      TEXT NOT NULL,
    PRIMARY KEY (message_id, user, emoji)
);
CREATE TABLE IF NOT EXISTS deleted (
    id           INTEGER NOT NULL,
    conversation INTEGER NOT NULL,
    seq          INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS deleted_by_seq ON deleted (seq);
CREATE TABLE IF NOT EXISTS reads (
    conversation INTEGER NOT NULL,
    user         TEXT NOT NULL,
    id           INTEGER NOT NULL,
    PRIMARY KEY (conversation, user)
);
CREATE TABLE IF NOT EXISTS state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS tls_identity (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    cert TEXT NOT NULL,
    key  TEXT NOT NULL
);
";

/// One stored message, before its reactions and reply are looked up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: i64,
    pub conversation: i64,
    pub author: String,
    pub text: String,
    pub timestamp: u64,
    pub edited: bool,
    pub reply_to: Option<i64>,
    pub pinned: bool,
}

/// A conversation between two accounts, `a` being the one whose id sorts first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub id: i64,
    pub a: String,
    pub b: String,
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// The name stored for an account, from the last time it was seen.
    pub fn name_of(&self, id: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT name FROM people WHERE id = ?1", params![id], |row| row.get(0))
            .optional()
    }

    /// Remembers an account's name, so messages can show it.
    pub fn remember_name(&self, id: &str, name: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO people (id, name) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET name = excluded.name",
            params![id, name],
        )?;
        Ok(())
    }

    /// The conversation between two accounts, if it exists.
    pub fn conversation_between(&self, x: &str, y: &str) -> rusqlite::Result<Option<Conversation>> {
        let (a, b) = if x <= y { (x, y) } else { (y, x) };
        self.conn
            .query_row(
                "SELECT id, a, b FROM conversations WHERE a = ?1 AND b = ?2",
                params![a, b],
                |row| Ok(Conversation { id: row.get(0)?, a: row.get(1)?, b: row.get(2)? }),
            )
            .optional()
    }

    /// The conversation between two accounts, made if it does not exist yet.
    pub fn open_conversation(&self, x: &str, y: &str) -> rusqlite::Result<Conversation> {
        if let Some(found) = self.conversation_between(x, y)? {
            return Ok(found);
        }
        let (a, b) = if x <= y { (x, y) } else { (y, x) };
        self.conn.execute("INSERT INTO conversations (a, b) VALUES (?1, ?2)", params![a, b])?;
        Ok(Conversation { id: self.conn.last_insert_rowid(), a: a.into(), b: b.into() })
    }

    /// One conversation by id.
    pub fn conversation(&self, id: i64) -> rusqlite::Result<Option<Conversation>> {
        self.conn
            .query_row("SELECT id, a, b FROM conversations WHERE id = ?1", params![id], |row| {
                Ok(Conversation { id: row.get(0)?, a: row.get(1)?, b: row.get(2)? })
            })
            .optional()
    }

    /// The conversations an account is in, with the id of their last message (0 for none).
    pub fn conversations_of(&self, user: &str) -> rusqlite::Result<Vec<(Conversation, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.a, c.b, COALESCE(MAX(m.timestamp), 0)
             FROM conversations c LEFT JOIN messages m ON m.conversation = c.id
             WHERE c.a = ?1 OR c.b = ?1
             GROUP BY c.id",
        )?;
        let rows = stmt.query_map(params![user], |row| {
            Ok((
                Conversation { id: row.get(0)?, a: row.get(1)?, b: row.get(2)? },
                row.get::<_, i64>(3)? as u64,
            ))
        })?;
        rows.collect()
    }

    /// The next change number. Every change to a message takes a new one, so a poll can ask for what came after.
    pub fn next_seq(&self) -> rusqlite::Result<i64> {
        self.conn.execute("INSERT OR IGNORE INTO state (key, value) VALUES ('seq', '0')", [])?;
        self.conn.execute(
            "UPDATE state SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT) WHERE key = 'seq'",
            [],
        )?;
        self.current_seq()
    }

    /// The latest change number handed out.
    pub fn current_seq(&self) -> rusqlite::Result<i64> {
        let value: Option<String> = self
            .conn
            .query_row("SELECT value FROM state WHERE key = 'seq'", [], |row| row.get(0))
            .optional()?;
        Ok(value.and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    /// Stores a new message and returns its id.
    pub fn insert_message(&self, row: &Row, seq: i64) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO messages (conversation, author, text, timestamp, edited, reply_to, pinned, seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                row.conversation,
                row.author,
                row.text,
                row.timestamp as i64,
                row.edited as i64,
                row.reply_to,
                row.pinned as i64,
                seq
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn message(&self, id: i64) -> rusqlite::Result<Option<Row>> {
        self.conn
            .query_row(
                "SELECT id, conversation, author, text, timestamp, edited, reply_to, pinned FROM messages WHERE id = ?1",
                params![id],
                row_from,
            )
            .optional()
    }

    /// The latest messages of a conversation, oldest first.
    pub fn history(&self, conversation: i64, limit: i64) -> rusqlite::Result<Vec<Row>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, conversation, author, text, timestamp, edited, reply_to, pinned FROM (
                SELECT * FROM messages WHERE conversation = ?1 ORDER BY id DESC LIMIT ?2
             ) ORDER BY id",
        )?;
        let rows = stmt.query_map(params![conversation, limit], row_from)?;
        rows.collect()
    }

    /// When the last message of a conversation was sent (Unix seconds), or 0 when there is none.
    pub fn last_timestamp(&self, conversation: i64) -> rusqlite::Result<u64> {
        let value: Option<i64> = self.conn.query_row(
            "SELECT MAX(timestamp) FROM messages WHERE conversation = ?1",
            params![conversation],
            |row| row.get(0),
        )?;
        Ok(value.unwrap_or(0) as u64)
    }

    pub fn pinned_count(&self, conversation: i64) -> rusqlite::Result<i64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE conversation = ?1 AND pinned = 1",
            params![conversation],
            |row| row.get(0),
        )
    }

    pub fn update_text(&self, id: i64, text: &str, seq: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE messages SET text = ?2, edited = 1, seq = ?3 WHERE id = ?1",
            params![id, text, seq],
        )?;
        Ok(())
    }

    pub fn set_pinned(&self, id: i64, pinned: bool, seq: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE messages SET pinned = ?2, seq = ?3 WHERE id = ?1",
            params![id, pinned as i64, seq],
        )?;
        Ok(())
    }

    pub fn touch(&self, id: i64, seq: i64) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE messages SET seq = ?2 WHERE id = ?1", params![id, seq])?;
        Ok(())
    }

    /// Removes a message and its reactions, and records that it was deleted for polls.
    pub fn delete_message(&self, id: i64, conversation: i64, seq: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM reactions WHERE message_id = ?1", params![id])?;
        self.conn.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
        self.conn.execute(
            "INSERT INTO deleted (id, conversation, seq) VALUES (?1, ?2, ?3)",
            params![id, conversation, seq],
        )?;
        Ok(())
    }

    /// Adds the reaction if it is not there, or removes it. Returns true when it was added.
    pub fn toggle_reaction(&self, id: i64, user: &str, emoji: &str) -> rusqlite::Result<bool> {
        let removed = self.conn.execute(
            "DELETE FROM reactions WHERE message_id = ?1 AND user = ?2 AND emoji = ?3",
            params![id, user, emoji],
        )?;
        if removed > 0 {
            return Ok(false);
        }
        self.conn.execute(
            "INSERT INTO reactions (message_id, user, emoji) VALUES (?1, ?2, ?3)",
            params![id, user, emoji],
        )?;
        Ok(true)
    }

    /// The reactions on a message, as (emoji, user) pairs in the order they were added.
    pub fn reactions(&self, id: i64) -> rusqlite::Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT emoji, user FROM reactions WHERE message_id = ?1 ORDER BY rowid")?;
        let rows = stmt.query_map(params![id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect()
    }

    pub fn set_read(&self, conversation: i64, user: &str, id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO reads (conversation, user, id) VALUES (?1, ?2, ?3)
             ON CONFLICT(conversation, user) DO UPDATE SET id = excluded.id",
            params![conversation, user, id],
        )?;
        Ok(())
    }

    /// The last message the account read in each of its conversations.
    pub fn reads_of(&self, user: &str) -> rusqlite::Result<Vec<(i64, i64)>> {
        let mut stmt = self.conn.prepare("SELECT conversation, id FROM reads WHERE user = ?1")?;
        let rows = stmt.query_map(params![user], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect()
    }

    /// Messages in the account's conversations that changed after `after`, oldest change first.
    pub fn changed_since(&self, user: &str, after: i64) -> rusqlite::Result<Vec<Row>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.conversation, m.author, m.text, m.timestamp, m.edited, m.reply_to, m.pinned
             FROM messages m JOIN conversations c ON c.id = m.conversation
             WHERE (c.a = ?1 OR c.b = ?1) AND m.seq > ?2
             ORDER BY m.seq",
        )?;
        let rows = stmt.query_map(params![user, after], row_from)?;
        rows.collect()
    }

    /// Messages deleted in the account's conversations after `after`, as (id, conversation) pairs.
    pub fn deleted_since(&self, user: &str, after: i64) -> rusqlite::Result<Vec<(i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.id, d.conversation FROM deleted d JOIN conversations c ON c.id = d.conversation
             WHERE (c.a = ?1 OR c.b = ?1) AND d.seq > ?2 ORDER BY d.seq",
        )?;
        let rows = stmt.query_map(params![user, after], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect()
    }

    pub fn tls_identity(&self) -> rusqlite::Result<Option<(String, String)>> {
        self.conn
            .query_row("SELECT cert, key FROM tls_identity WHERE id = 1", [], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()
    }

    pub fn store_tls_identity(&self, cert: &str, key: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO tls_identity (id, cert, key) VALUES (1, ?1, ?2)",
            params![cert, key],
        )?;
        Ok(())
    }

    /// The certificate fingerprint remembered for a service, if one was seen.
    pub fn pin(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.state(&format!("pin:{key}"))
    }

    pub fn set_pin(&self, key: &str, fingerprint: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO state (key, value) VALUES (?1, ?2)",
            params![format!("pin:{key}"), fingerprint],
        )?;
        Ok(())
    }

    fn state(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn.query_row("SELECT value FROM state WHERE key = ?1", params![key], |row| row.get(0)).optional()
    }
}

fn row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        id: row.get(0)?,
        conversation: row.get(1)?,
        author: row.get(2)?,
        text: row.get(3)?,
        timestamp: row.get::<_, i64>(4)? as u64,
        edited: row.get::<_, i64>(5)? != 0,
        reply_to: row.get(6)?,
        pinned: row.get::<_, i64>(7)? != 0,
    })
}
