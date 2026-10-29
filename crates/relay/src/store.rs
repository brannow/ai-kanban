//! The relay's own database. Never the kanban store.
//!
//! # Why a separate schema
//!
//! The server stores who may write and an ordered log of changes it does not read. Putting
//! that into core's schema would tie the relay to every core migration, and put empty server
//! tables into every personal store for no reader. It has its own `user_version` chain and its
//! own `application_id`, and it refuses a file that carries neither -- so pointing `--db` at a
//! personal `kanban.db` fails at startup instead of writing server tables into it.

use ai_kanban_protocol::{self as protocol, Change, HighWater, Joined, Member, Page, Pulled, Pushed, Ref, RoomInfo};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

/// Unix epoch seconds, as everywhere in ai-kanban.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
use std::path::{Path, PathBuf};

/// Marks a file as a relay database. Arbitrary, fixed forever; `PRAGMA application_id` exists
/// for exactly this. ("AKbS" in ASCII.)
const APPLICATION_ID: i64 = 0x414B_6253;

/// Each entry runs once, in order, inside a transaction, and stamps `user_version`. Frozen once
/// shipped, for the reason note #178 gives about core's migrations: a store that already ran
/// one never runs an edited version of it.
const MIGRATIONS: &[&str] = &[
    include_str!("schema.sql"),
    include_str!("migrate_002_room_board.sql"),
    include_str!("migrate_003_keys_and_names.sql"),
    include_str!("migrate_004_key_client.sql"),
];

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("missing or unknown key")]
    Unauthorized,
    #[error("{0}")]
    BadRequest(String),
    /// The client says it speaks a protocol older than this relay accepts (426).
    #[error("{0}")]
    UpgradeRequired(String),
    #[error("{0}")]
    NotFound(String),
    /// The request contradicts what the server already holds -- a reused change id, a tag
    /// someone else owns.
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, ServerError>;

/// Who a request is from, resolved from its key. `user` is the name its client chose for
/// itself -- a claim, not a proof; `tag` is what the relay vouches for.
#[derive(Debug, Clone)]
pub struct Principal {
    pub user: String,
    pub tag: String,
    pub key_id: i64,
}

#[derive(Debug, Clone)]
pub struct KeyRow {
    pub id: i64,
    /// None until the key is first used.
    pub tag: Option<String>,
    /// The name its client chose, if it has a tag.
    pub name: Option<String>,
    /// The admin's note of who it was given to.
    pub label: String,
    /// The `User-Agent` its machine last sent, if any.
    pub client: Option<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

/// What `forget` and `delete_room` would remove, so the CLI can show it before `--yes`.
#[derive(Debug, Clone, Default)]
pub struct Removal {
    pub changes: i64,
    pub members: i64,
}

pub struct ServerDb {
    pub conn: Connection,
    path: Option<PathBuf>,
}

impl ServerDb {
    /// Where the relay keeps its database unless told otherwise. `/data` is the container's
    /// volume. Deliberately no fallback to `AI_KANBAN_DB` or the platform data directory: a
    /// server started on a laptop must never open the personal store.
    pub fn default_path() -> PathBuf {
        std::env::var_os("AI_KANBAN_SERVER_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/data/server.db"))
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn, Some(path.to_path_buf()))
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?, None)
    }

    fn init(conn: Connection, path: Option<PathBuf>) -> Result<Self> {
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        let app: i64 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let tables: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table'", [], |r| r.get(0))?;
        if app != APPLICATION_ID && tables > 0 {
            return Err(ServerError::Other(
                "this file is not an ai-kanban-relay database (is it a personal kanban.db?). \
                 Point --db or AI_KANBAN_SERVER_DB at a new file."
                    .into(),
            ));
        }
        let mut version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version as usize > MIGRATIONS.len() {
            return Err(ServerError::Other(format!(
                "server database is version {version}, newer than this binary ({}). Upgrade the image.",
                MIGRATIONS.len()
            )));
        }
        // Off while migrating, outside any transaction (inside one the pragma does nothing):
        // a migration that rebuilds a table drops the old one while others still point at it.
        // Checked once all have run, so a migration cannot leave a dangling reference behind.
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        while (version as usize) < MIGRATIONS.len() {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(MIGRATIONS[version as usize])?;
            version += 1;
            tx.pragma_update(None, "user_version", version)?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.commit()?;
        }
        let dangling: i64 = conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| r.get(0))?;
        if dangling > 0 {
            return Err(ServerError::Other(format!("server database has {dangling} broken references after migrating")));
        }
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self { conn, path })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Users, keys, tags
// ---------------------------------------------------------------------------

/// 32 random bytes as hex, prefixed so a key pasted into the wrong place is recognisable.
fn new_key() -> Result<String> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| ServerError::Other(format!("no randomness: {e}")))?;
    Ok(format!("akb_{}", hex(&buf)))
}

/// SHA-256, not a slow password hash: a key is 256 random bits, so there is nothing for
/// argon2 to protect against, and every request would pay for it.
fn hash_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(key.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Text that other people's terminals will print: no control characters, trimmed, at most `max`
/// characters. Cleaned rather than refused, where refusing would fail a join over a hint.
fn clean_text(raw: &str, max: usize) -> String {
    raw.chars().filter(|c| !c.is_control()).take(max).collect::<String>().trim().to_string()
}

/// A name as the team will see it, or why not. Shown in other people's terminals and pages, so
/// what it may contain is a list, not a list of exceptions: letters and digits of any script,
/// the marks those scripts write their letters with, single spaces, and `- . _ '` (and the two
/// middle dots names use). That leaves out by itself what has no business in a name: emoji and
/// symbols, control characters (terminal escapes), invisible format characters (zero-width
/// joiners, the bidi overrides that make text read differently than it is stored), and the
/// characters of markup (`< > & "`).
///
/// Refused rather than cleaned: a name that silently came out different from the one typed is
/// worse than being told. Only spaces are tidied -- trimmed, and runs of them made one.
fn valid_name(raw: &str) -> Result<String> {
    let name = raw.split(' ').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
    let refuse = |why: String| Err(ServerError::BadRequest(format!("{why}. A name is letters and digits of any script, spaces, and - . _ '")));
    if let Some(c) = name.chars().find(|c| !name_char(*c)) {
        let shown = if c.is_control() || c.is_whitespace() || !c.is_alphanumeric() && c as u32 >= 0x2000 && c as u32 <= 0x206F {
            format!("U+{:04X}", c as u32)
        } else {
            format!("\"{c}\" (U+{:04X})", c as u32)
        };
        return refuse(format!("a name cannot contain {shown}"));
    }
    if !name.chars().any(char::is_alphanumeric) {
        return refuse("a name needs at least one letter or digit".into());
    }
    if name.chars().count() > 32 {
        return refuse("a name is at most 32 characters".into());
    }
    Ok(name)
}

fn name_char(c: char) -> bool {
    if c.is_alphanumeric() || matches!(c, ' ' | '-' | '.' | '_' | '\'' | '·' | '・') {
        return true;
    }
    // The marks scripts write letters with, which the standard library does not classify:
    // accents typed separately, and the vowel signs and viramas of Hebrew, Arabic, the Indic
    // scripts, Thai, Lao, Tibetan, Myanmar and Khmer. Without them `लक्ष्मी` or `ศักดิ์` would be
    // refused. Whole blocks, since everything else in them is a letter anyway.
    matches!(c as u32,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05C7 | 0x0610..=0x061A | 0x064B..=0x065F | 0x0670
        | 0x06D6..=0x06ED | 0x0900..=0x0DFF | 0x0E00..=0x0EFF | 0x0F00..=0x0FFF | 0x1000..=0x109F
        | 0x1780..=0x17FF | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x3099..=0x309A | 0xFE20..=0xFE2F)
}

/// The letter a tag is made from: the name's first letter, as the plain letter people would
/// write it with on an ASCII keyboard (`ßaskia` s, `Örkan` o, `Дмитрий` d, `Γιώργος` g). Scripts
/// without such a letter -- Chinese, Japanese, Korean, Thai, ... -- would need a romanization
/// dictionary for one; for those the first Latin letter elsewhere in the name serves (`王伟 Wang`
/// w), else `x`. The tag only shows in handles (`#12-x1`); the team sees the name.
fn initial(name: &str) -> char {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .and_then(plain_letter)
        .or_else(|| name.chars().find(char::is_ascii_alphabetic).map(|c| c.to_ascii_lowercase()))
        .unwrap_or('x')
}

fn plain_letter(c: char) -> Option<char> {
    if c.is_ascii_alphabetic() {
        return Some(c.to_ascii_lowercase());
    }
    let l = c.to_lowercase().next()?;
    Some(match l {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' | 'ǎ' | 'æ' => 'a',
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
        'ď' | 'đ' | 'ð' => 'd',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
        'ĥ' | 'ħ' => 'h',
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => 'i',
        'ĵ' => 'j',
        'ķ' => 'k',
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => 'l',
        'ñ' | 'ń' | 'ņ' | 'ň' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' | 'œ' => 'o',
        'ŕ' | 'ŗ' | 'ř' => 'r',
        'ß' | 'ś' | 'ŝ' | 'ş' | 'š' | 'ș' => 's',
        'ţ' | 'ť' | 'ŧ' | 'ț' | 'þ' => 't',
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
        'ŵ' => 'w',
        'ý' | 'ÿ' | 'ŷ' => 'y',
        'ź' | 'ż' | 'ž' => 'z',
        // Greek
        'α' | 'ά' => 'a', 'β' => 'b', 'γ' => 'g', 'δ' => 'd', 'ε' | 'έ' => 'e', 'ζ' => 'z',
        'η' | 'ή' | 'ι' | 'ί' | 'ϊ' => 'i', 'θ' | 'τ' => 't', 'κ' => 'k', 'λ' => 'l', 'μ' => 'm',
        'ν' => 'n', 'ξ' => 'x', 'ο' | 'ό' | 'ω' | 'ώ' => 'o', 'π' | 'ψ' => 'p', 'ρ' => 'r',
        'σ' | 'ς' => 's', 'υ' | 'ύ' | 'ϋ' => 'y', 'φ' => 'f', 'χ' => 'c',
        // Cyrillic
        'а' => 'a', 'б' => 'b', 'в' => 'v', 'г' | 'ґ' => 'g', 'д' => 'd', 'е' | 'ё' | 'є' | 'э' => 'e',
        'ж' | 'з' => 'z', 'и' | 'й' | 'і' | 'ї' => 'i', 'к' => 'k', 'л' => 'l', 'м' => 'm', 'н' => 'n',
        'о' => 'o', 'п' => 'p', 'р' => 'r', 'с' | 'ш' | 'щ' => 's', 'т' => 't', 'у' => 'u', 'ф' => 'f',
        'х' => 'h', 'ц' | 'ч' => 'c', 'ы' | 'ю' | 'я' => 'y',
        _ => return None,
    })
}

/// A name for a machine that never chose one, like `brave-otter`: something to tell people apart
/// by until they pick their own.
fn generated_name() -> Result<String> {
    const ADJ: &[&str] = &[
        "brave", "calm", "eager", "fond", "glad", "keen", "lucky", "merry", "nimble", "quiet",
        "rapid", "sunny", "tidy", "witty", "bold", "clever",
    ];
    const ANIMAL: &[&str] = &[
        "otter", "badger", "heron", "lynx", "marten", "osprey", "puffin", "raven", "stoat",
        "tern", "vole", "wren", "beaver", "crane", "falcon", "hare",
    ];
    let mut b = [0u8; 2];
    getrandom::fill(&mut b).map_err(|e| ServerError::Other(format!("no randomness: {e}")))?;
    Ok(format!("{}-{}", ADJ[b[0] as usize % ADJ.len()], ANIMAL[b[1] as usize % ANIMAL.len()]))
}

impl ServerDb {
    /// A key for one machine, to hand to whoever runs it. It has no tag yet: the tag is fixed
    /// the first time the key is used, from the name its client chooses (`name_key`), or a
    /// generated one. `label` is the admin's own note of who it was given to; the team never
    /// sees it. Returns the key's id and the key, which exists only in this return value.
    pub fn issue_key(&self, label: &str) -> Result<(i64, String)> {
        let label = clean_text(label, 64);
        if label.is_empty() {
            return Err(ServerError::BadRequest("a key needs a label: who it is for, like \"volker laptop\"".into()));
        }
        let key = new_key()?;
        self.conn.execute("INSERT INTO keys (tag, label, hash, created_at) VALUES (NULL, ?1, ?2, ?3)", params![label, hash_key(&key), now()])?;
        Ok((self.conn.last_insert_rowid(), key))
    }

    /// A key already named and under a given tag, without the first-use step. For tests and
    /// tools that need a known tag; the admin commands go through `issue_key`.
    pub fn add_key(&self, name: &str, tag: &str) -> Result<String> {
        if !protocol::valid_tag(tag) {
            return Err(ServerError::BadRequest(format!(
                "tag {tag:?}: a lowercase letter, then up to 7 lowercase letters or digits (no '-')"
            )));
        }
        if tag == protocol::SERVER_TAG {
            return Err(ServerError::BadRequest(format!("tag {tag:?} is reserved for the server")));
        }
        let tx = self.conn.unchecked_transaction()?;
        let taken: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM tags WHERE tag = ?1)", [tag], |r| r.get(0))?;
        if taken {
            return Err(ServerError::Conflict(format!("tag {tag} is taken. Tags are never reused; `key rotate {tag}` gives it a new key")));
        }
        tx.execute("INSERT INTO tags (tag, name, created_at) VALUES (?1, ?2, ?3)", params![tag, name, now()])?;
        let key = new_key()?;
        tx.execute("INSERT INTO keys (tag, label, hash, created_at) VALUES (?1, ?2, ?3, ?4)", params![tag, name, hash_key(&key), now()])?;
        tx.commit()?;
        Ok(key)
    }

    /// Sets the name the key's machine goes by. The first time, this fixes its tag: the name's
    /// initial and the lowest free number (`Volker` gets `v1`, the next `v` name `v2`). After
    /// that the tag never changes -- handles like `#122-v1` are published under it -- and only
    /// the name does. Names may repeat; tags tell their owners apart.
    pub fn name_key(&self, key_id: i64, name: &str) -> Result<Principal> {
        let name = valid_name(name)?;
        let tx = self.conn.unchecked_transaction()?;
        let tag: Option<String> = tx.query_row("SELECT tag FROM keys WHERE id = ?1", [key_id], |r| r.get(0))?;
        let tag = match tag {
            Some(tag) => {
                tx.execute("UPDATE tags SET name = ?2 WHERE tag = ?1", params![tag, name])?;
                tag
            }
            None => {
                let initial = initial(&name);
                let mut n = 1;
                let tag = loop {
                    let tag = format!("{initial}{n}");
                    let taken: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM tags WHERE tag = ?1)", [&tag], |r| r.get(0))?;
                    if !taken && tag != protocol::SERVER_TAG {
                        break tag;
                    }
                    n += 1;
                };
                tx.execute("INSERT INTO tags (tag, name, created_at) VALUES (?1, ?2, ?3)", params![tag, name, now()])?;
                tx.execute("UPDATE keys SET tag = ?2 WHERE id = ?1", params![key_id, tag])?;
                tag
            }
        };
        tx.commit()?;
        Ok(Principal { user: name, tag, key_id })
    }

    /// The live key a selector means: a key id (`3`) or the tag it serves (`v1`). An unused key
    /// has no tag yet, so it can only be named by its id.
    pub fn key_id(&self, selector: &str) -> Result<i64> {
        let found: Option<i64> = match selector.parse::<i64>() {
            Ok(id) => self.conn.query_row("SELECT id FROM keys WHERE id = ?1 AND revoked_at IS NULL", [id], |r| r.get(0)).optional()?,
            Err(_) => self.conn.query_row("SELECT id FROM keys WHERE tag = ?1 AND revoked_at IS NULL", [selector], |r| r.get(0)).optional()?,
        };
        found.ok_or_else(|| ServerError::NotFound(format!("no live key {selector} (a key id or a tag, from key list)")))
    }

    /// A new key in place of a lost or leaked one: same tag, name and label, so everything the
    /// machine published keeps its handles. The old key stops working at once.
    pub fn rotate_key(&self, key_id: i64) -> Result<(Option<String>, String)> {
        let tx = self.conn.unchecked_transaction()?;
        let (tag, label): (Option<String>, String) = tx.query_row("SELECT tag, label FROM keys WHERE id = ?1", [key_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        tx.execute("UPDATE keys SET revoked_at = ?2 WHERE id = ?1", params![key_id, now()])?;
        let key = new_key()?;
        tx.execute("INSERT INTO keys (tag, label, hash, created_at) VALUES (?1, ?2, ?3, ?4)", params![tag, label, hash_key(&key), now()])?;
        tx.commit()?;
        Ok((tag, key))
    }

    pub fn revoke_key(&self, id: i64) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE keys SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
            params![id, now()],
        )?;
        if n == 0 {
            return Err(ServerError::NotFound(format!("no live key {id}")));
        }
        Ok(())
    }

    pub fn label_key(&self, id: i64, label: &str) -> Result<()> {
        let label = clean_text(label, 64);
        if label.is_empty() {
            return Err(ServerError::BadRequest("a label needs at least one visible character".into()));
        }
        self.conn.execute("UPDATE keys SET label = ?2 WHERE id = ?1", params![id, label])?;
        Ok(())
    }

    pub fn keys(&self) -> Result<Vec<KeyRow>> {
        let mut st = self.conn.prepare(
            "SELECT k.id, k.tag, t.name, k.label, k.created_at, k.last_used_at, k.revoked_at, k.client
               FROM keys k LEFT JOIN tags t ON t.tag = k.tag
              ORDER BY k.revoked_at IS NOT NULL, k.id",
        )?;
        let rows = st.query_map([], |r| {
            Ok(KeyRow {
                id: r.get(0)?,
                tag: r.get(1)?,
                name: r.get(2)?,
                label: r.get(3)?,
                created_at: r.get(4)?,
                last_used_at: r.get(5)?,
                revoked_at: r.get(6)?,
                client: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Records what a key's machine says it runs (its `User-Agent`). Written only when it
    /// changed, so a client's every pull is not also a write.
    pub fn saw_client(&self, key_id: i64, agent: &str) -> Result<()> {
        let agent = clean_text(agent, 80);
        self.conn.execute("UPDATE keys SET client = ?2 WHERE id = ?1 AND client IS NOT ?2", params![key_id, agent])?;
        Ok(())
    }

    /// The live key a bearer key is, without binding anything: the key's id and its tag and
    /// name if it has them. For the naming route, which must run before the tag is fixed, and
    /// for the debug log, which must not change what it watches.
    pub fn lookup(&self, key: &str) -> Result<(i64, Option<(String, String)>)> {
        let row: Option<(i64, Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT k.id, k.tag, t.name FROM keys k LEFT JOIN tags t ON t.tag = k.tag
                  WHERE k.hash = ?1 AND k.revoked_at IS NULL",
                [hash_key(key.trim())],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (id, tag, name) = row.ok_or(ServerError::Unauthorized)?;
        Ok((id, tag.zip(name)))
    }

    /// The principal a bearer key belongs to. A key used for the first time without having
    /// been named gets a generated name, and with it its tag: a client from before names
    /// (0.1.x) never sends one, and must still work with a key issued since.
    pub fn authenticate(&self, key: &str) -> Result<Principal> {
        let (key_id, named) = self.lookup(key)?;
        let p = match named {
            Some((tag, user)) => Principal { user, tag, key_id },
            None => self.name_key(key_id, &generated_name()?)?,
        };
        // Minute resolution is plenty for "when was this key last seen", and it keeps a
        // chatty client from turning every pull into a write.
        self.conn.execute(
            "UPDATE keys SET last_used_at = ?2 WHERE id = ?1 AND (last_used_at IS NULL OR last_used_at < ?2 - 60)",
            params![key_id, now()],
        )?;
        Ok(p)
    }

    pub fn high_water(&self, tag: &str) -> Result<HighWater> {
        let mut hw = HighWater::default();
        let mut st = self.conn.prepare("SELECT kind, value FROM high_water WHERE tag = ?1")?;
        let rows = st.query_map([tag], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (kind, value) = row?;
            match kind.as_str() {
                "outbox" => hw.outbox = value,
                "task" => hw.task = value,
                "note" => hw.note = value,
                "event" => hw.event = value,
                _ => {}
            }
        }
        Ok(hw)
    }
}

// ---------------------------------------------------------------------------
// Rooms
// ---------------------------------------------------------------------------

/// A board name as other people's terminals will show it.
fn board_label(raw: &str) -> String {
    clean_text(raw, 64)
}

fn parse_room(raw: &str) -> Result<String> {
    protocol::room_name(raw).ok_or_else(|| {
        ServerError::BadRequest(format!(
            "room {raw:?}: up to 64 lowercase letters, digits, '-' or '_', starting with a letter or digit"
        ))
    })
}

impl ServerDb {
    fn room_id(&self, name: &str) -> Result<i64> {
        let name = parse_room(name)?;
        self.conn
            .query_row("SELECT id FROM rooms WHERE name = ?1", [&name], |r| r.get(0))
            .optional()?
            .ok_or_else(|| ServerError::NotFound(format!("no room {name}")))
    }

    /// The room, if the principal is a current member of it. Writing requires having joined, so
    /// membership is what `room list` and the empty-room rule can trust. Reading does not: any
    /// key holder may join any room anyway, and a look before connecting (`room show`) should
    /// not put the looker in the member list.
    fn member_room(&self, name: &str, p: &Principal) -> Result<i64> {
        let id = self.room_id(name)?;
        let member: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM members WHERE room_id = ?1 AND tag = ?2 AND left_at IS NULL)",
            params![id, p.tag],
            |r| r.get(0),
        )?;
        if !member {
            return Err(ServerError::NotFound(format!(
                "not a member of room {name}. Connect a board to it first"
            )));
        }
        Ok(id)
    }

    pub fn room_info(&self, id: i64) -> Result<RoomInfo> {
        let (name, created_at, created_by, board): (String, i64, String, Option<String>) = self.conn.query_row(
            "SELECT name, created_at, created_by, board FROM rooms WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let mut st = self.conn.prepare(
            "SELECT m.tag, t.name FROM members m JOIN tags t ON t.tag = m.tag
              WHERE m.room_id = ?1 AND m.left_at IS NULL ORDER BY m.tag",
        )?;
        let members = st
            .query_map([id], |r| Ok(Member { tag: r.get(0)?, user: r.get(1)? }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let (changes, head, last_change_at): (i64, i64, Option<i64>) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(MAX(seq), 0), MAX(received_at) FROM changes WHERE room_id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(RoomInfo { name, created_at, created_by, members, changes, head, board, last_change_at })
    }

    pub fn rooms(&self) -> Result<Vec<RoomInfo>> {
        let ids: Vec<i64> = {
            let mut st = self.conn.prepare("SELECT id FROM rooms ORDER BY name")?;
            st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        ids.into_iter().map(|id| self.room_info(id)).collect()
    }

    /// Joins a room, creating it if it does not exist. Any user may create one: a server is a
    /// team. Joining again is harmless.
    pub fn join(&self, name: &str, p: &Principal) -> Result<Joined> {
        self.join_from(name, p, None)
    }

    /// `join`, naming the board the room is created from. Only the join that creates the room
    /// records it: a later member's board name says nothing about where the room came from.
    pub fn join_from(&self, name: &str, p: &Principal, board: Option<&str>) -> Result<Joined> {
        let name = parse_room(name)?;
        let board = board.map(board_label).filter(|b| !b.is_empty());
        let tx = self.conn.unchecked_transaction()?;
        let created = tx.execute(
            "INSERT OR IGNORE INTO rooms (name, created_at, created_by, board) VALUES (?1, ?2, ?3, ?4)",
            params![name, now(), p.user, board],
        )? == 1;
        let id: i64 = tx.query_row("SELECT id FROM rooms WHERE name = ?1", [&name], |r| r.get(0))?;
        tx.execute(
            "INSERT INTO members (room_id, tag, joined_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (room_id, tag) DO UPDATE SET left_at = NULL",
            params![id, p.tag, now()],
        )?;
        tx.commit()?;
        Ok(Joined { room: self.room_info(id)?, created })
    }

    pub fn leave(&self, name: &str, p: &Principal) -> Result<()> {
        let id = self.member_room(name, p)?;
        self.conn.execute(
            "UPDATE members SET left_at = ?3 WHERE room_id = ?1 AND tag = ?2",
            params![id, p.tag, now()],
        )?;
        Ok(())
    }

    /// What deleting a room would remove.
    pub fn room_removal(&self, name: &str) -> Result<Removal> {
        let id = self.room_id(name)?;
        let changes = self.conn.query_row("SELECT COUNT(*) FROM changes WHERE room_id = ?1", [id], |r| r.get(0))?;
        let members = self.conn.query_row(
            "SELECT COUNT(*) FROM members WHERE room_id = ?1 AND left_at IS NULL",
            [id],
            |r| r.get(0),
        )?;
        Ok(Removal { changes, members })
    }

    pub fn delete_room(&self, name: &str) -> Result<()> {
        let id = self.room_id(name)?;
        let tx = self.conn.unchecked_transaction()?;
        // Explicit rather than trusting the cascade, as core's forget does: a delete that
        // leaves content behind because a pragma did not stick is the failure to avoid.
        tx.execute("DELETE FROM changes WHERE room_id = ?1", [id])?;
        tx.execute("DELETE FROM forgotten WHERE room_id = ?1", [id])?;
        tx.execute("DELETE FROM members WHERE room_id = ?1", [id])?;
        tx.execute("DELETE FROM rooms WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Removes rooms that hold nothing and that nobody is connected to, once they have been
    /// that way for `max_age` seconds. A typo'd `room connect` creates a room; this is what
    /// keeps those from piling up. A room with content is never removed this way, connected
    /// or not -- it may be the only copy of someone's history.
    pub fn remove_empty_rooms(&self, max_age: i64) -> Result<Vec<String>> {
        let cutoff = now() - max_age;
        let names: Vec<String> = {
            let mut st = self.conn.prepare(
                "SELECT r.name FROM rooms r
                  WHERE NOT EXISTS (SELECT 1 FROM changes c WHERE c.room_id = r.id)
                    AND NOT EXISTS (SELECT 1 FROM members m WHERE m.room_id = r.id AND m.left_at IS NULL)
                    AND MAX(r.created_at, COALESCE((SELECT MAX(left_at) FROM members m WHERE m.room_id = r.id), 0)) < ?1
                  ORDER BY r.name",
            )?;
            st.query_map([cutoff], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for name in &names {
            self.delete_room(name)?;
        }
        Ok(names)
    }
}

// ---------------------------------------------------------------------------
// The change log
// ---------------------------------------------------------------------------

fn body_hash(body: &str) -> String {
    hash_key(body)
}

impl ServerDb {
    /// Stores a batch, all or nothing.
    ///
    /// What the relay checks without reading values: the change id and the clock carry the
    /// key's own tag, so nobody can write under someone else's name or win a conflict with
    /// someone else's tag. Everything else is stored as sent.
    pub fn push(&self, room: &str, p: &Principal, changes: &[Change]) -> Result<Pushed> {
        if changes.len() > protocol::MAX_BATCH {
            return Err(ServerError::BadRequest(format!(
                "{} changes in one push; the limit is {}",
                changes.len(),
                protocol::MAX_BATCH
            )));
        }
        let room_id = self.member_room(room, p)?;
        for (i, ch) in changes.iter().enumerate() {
            let bad = |why: String| ServerError::BadRequest(format!("change {i} ({}): {why}", ch.cid));
            match ch.cid_parts() {
                Some((tag, _)) if tag == p.tag => {}
                Some((tag, _)) => return Err(bad(format!("change id carries tag {tag}, but this key is {}", p.tag))),
                None => return Err(bad("change id must be <tag>:<positive number>".into())),
            }
            if ch.clock.tag != p.tag {
                return Err(bad(format!("clock carries tag {}, but this key is {}", ch.clock.tag, p.tag)));
            }
            if !ch.entity.valid() {
                return Err(bad(format!("entity must be one of {:?} with a valid origin and a positive id", protocol::KINDS)));
            }
            if ch.subject.as_ref().is_some_and(|s| !s.valid() || s.kind == "event") {
                return Err(bad("subject must be a task or note with a valid origin and a positive id".into()));
            }
        }

        let mut out = Pushed::default();
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        for ch in changes {
            if self.is_forgotten(room_id, &ch.entity)? || ch.subject.as_ref().map(|s| self.is_forgotten(room_id, s)).transpose()?.unwrap_or(false) {
                out.forgotten += 1;
                continue;
            }
            let body = serde_json::to_string(ch).map_err(|e| ServerError::Other(e.to_string()))?;
            let hash = body_hash(&body);
            let existing: Option<String> = tx
                .query_row("SELECT body_hash FROM changes WHERE cid = ?1", [&ch.cid], |r| r.get(0))
                .optional()?;
            match existing {
                Some(h) if h == hash => {
                    out.duplicates += 1;
                    continue;
                }
                Some(_) => {
                    return Err(ServerError::Conflict(format!(
                        "change id {} was already used for different content. This store looks restored \
                         from a backup or copied, or a second machine sharing one key: it needs a key of its own \
                         (`ai-kanban-relay key add <label>`)",
                        ch.cid
                    )));
                }
                None => {}
            }
            let (s_kind, s_origin, s_id) = match &ch.subject {
                Some(s) => (Some(s.kind.as_str()), Some(s.origin.as_str()), Some(s.id)),
                None => (None, None, None),
            };
            tx.execute(
                "INSERT INTO changes (room_id, cid, author, tag, received_at, entity_kind, entity_origin, entity_id,
                                      subject_kind, subject_origin, subject_id, body, body_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![room_id, ch.cid, p.user, p.tag, now(), ch.entity.kind, ch.entity.origin, ch.entity.id,
                        s_kind, s_origin, s_id, body, hash],
            )?;
            if let Some((_, seq)) = ch.cid_parts() {
                bump(&tx, &p.tag, "outbox", seq)?;
            }
            if ch.entity.origin == p.tag {
                bump(&tx, &p.tag, &ch.entity.kind, ch.entity.id)?;
            }
            out.accepted += 1;
        }
        tx.commit()?;
        out.head = self.head(room_id)?;
        Ok(out)
    }

    fn is_forgotten(&self, room_id: i64, r: &Ref) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM forgotten WHERE room_id = ?1 AND kind = ?2 AND origin = ?3 AND id = ?4)",
            params![room_id, r.kind, r.origin, r.id],
            |row| row.get(0),
        )?)
    }

    pub fn head(&self, room_id: i64) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM changes WHERE room_id = ?1", [room_id], |r| r.get(0))?)
    }

    /// The head of a room the principal is a member of. What the notification stream starts from.
    pub fn member_head(&self, room: &str, p: &Principal) -> Result<(i64, i64)> {
        let id = self.member_room(room, p)?;
        Ok((id, self.head(id)?))
    }

    /// Open to every key, member or not (see `member_room`). `p` is the authenticated caller.
    pub fn pull(&self, room: &str, _p: &Principal, since: i64, limit: usize) -> Result<Page> {
        let room_id = self.room_id(room)?;
        let limit = limit.clamp(1, protocol::MAX_PAGE);
        let mut st = self.conn.prepare(
            "SELECT seq, author, received_at, body FROM changes WHERE room_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = st.query_map(params![room_id, since, limit as i64 + 1], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?))
        })?;
        let mut changes = Vec::new();
        for row in rows {
            let (seq, author, received_at, body) = row?;
            let change: Change = serde_json::from_str(&body).map_err(|e| ServerError::Other(format!("stored change {seq}: {e}")))?;
            changes.push(Pulled { seq, author, received_at, change });
        }
        let more = changes.len() > limit;
        changes.truncate(limit);
        Ok(Page { changes, head: self.head(room_id)?, more })
    }

    /// What forgetting `r` in `room` would remove.
    pub fn forget_removal(&self, room: &str, r: &Ref) -> Result<Removal> {
        let room_id = self.room_id(room)?;
        let changes = self.conn.query_row(
            "SELECT COUNT(*) FROM changes WHERE room_id = ?1 AND
                ((entity_kind = ?2 AND entity_origin = ?3 AND entity_id = ?4) OR
                 (subject_kind = ?2 AND subject_origin = ?3 AND subject_id = ?4))",
            params![room_id, r.kind, r.origin, r.id],
            |row| row.get(0),
        )?;
        Ok(Removal { changes, members: 0 })
    }

    /// Permanently removes a task or note from a room, for every member.
    ///
    /// Deletes every change whose entity or subject it is -- the same set `core::forget`
    /// deletes locally, because the subject was chosen to mirror it. Then it appends a tombstone
    /// carrying no content, signed with the server's tag, so every member applies the forget,
    /// and remembers the subject, so a member who was offline cannot push it back.
    pub fn forget(&self, room: &str, r: &Ref) -> Result<Removal> {
        if r.kind == "event" || !r.valid() {
            return Err(ServerError::BadRequest("only a task or a note can be forgotten".into()));
        }
        let room_id = self.room_id(room)?;
        // Forgetting twice is a no-op, not a second tombstone: nothing is left to remove, and
        // every member already has the first one.
        if self.is_forgotten(room_id, r)? {
            return Ok(Removal::default());
        }
        let removal = self.forget_removal(room, r)?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM changes WHERE room_id = ?1 AND
                ((entity_kind = ?2 AND entity_origin = ?3 AND entity_id = ?4) OR
                 (subject_kind = ?2 AND subject_origin = ?3 AND subject_id = ?4))",
            params![room_id, r.kind, r.origin, r.id],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO forgotten (room_id, kind, origin, id, at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![room_id, r.kind, r.origin, r.id, now()],
        )?;
        let seq = bump_next(&tx, protocol::SERVER_TAG, "outbox")?;
        let tomb = Change {
            cid: format!("{}:{seq}", protocol::SERVER_TAG),
            entity: r.clone(),
            subject: None,
            field: Some("forgotten".into()),
            value: serde_json::Value::Bool(true),
            clock: protocol::Clock {
                wall_ms: now() * 1000,
                counter: 0,
                tag: protocol::SERVER_TAG.into(),
            },
            extra: Default::default(),
        };
        let body = serde_json::to_string(&tomb).map_err(|e| ServerError::Other(e.to_string()))?;
        tx.execute(
            "INSERT INTO changes (room_id, cid, author, tag, received_at, entity_kind, entity_origin, entity_id, body, body_hash)
             VALUES (?1, ?2, 'admin', ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![room_id, tomb.cid, protocol::SERVER_TAG, now(), r.kind, r.origin, r.id, body, body_hash(&body)],
        )?;
        tx.commit()?;
        Ok(removal)
    }

    /// A consistent copy of the database, through SQLite so the WAL comes with it (the reason
    /// `core::transfer` exists, applied to the relay).
    pub fn snapshot(&self, dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        if dest.exists() {
            return Err(ServerError::Other(format!("{} exists", dest.display())));
        }
        self.conn.execute("VACUUM INTO ?1", [dest.to_string_lossy()])?;
        Ok(())
    }
}

/// Raises a tag's high-water mark for `kind` to at least `value`.
fn bump(tx: &Connection, tag: &str, kind: &str, value: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO high_water (tag, kind, value) VALUES (?1, ?2, ?3)
         ON CONFLICT (tag, kind) DO UPDATE SET value = MAX(value, excluded.value)",
        params![tag, kind, value],
    )?;
    Ok(())
}

/// The next value of a counter the server owns, for its own change ids.
fn bump_next(tx: &Connection, tag: &str, kind: &str) -> Result<i64> {
    tx.execute(
        "INSERT INTO high_water (tag, kind, value) VALUES (?1, ?2, 1)
         ON CONFLICT (tag, kind) DO UPDATE SET value = value + 1",
        params![tag, kind],
    )?;
    Ok(tx.query_row("SELECT value FROM high_water WHERE tag = ?1 AND kind = ?2", params![tag, kind], |r| r.get(0))?)
}
