//! Text-vector cache: `.index/vectors.sqlite` (plan §9.1).
//!
//! Vectors are derived data keyed by (text-model key, SHA-256 of a window's
//! text), so they survive `index rebuild` and a speaker or person rename
//! never needs a new embedding. Deleting `.index/` only costs recomputing
//! them. Vectors are stored L2-normalised as little-endian `f32`.
//!
//! The cache uses a rollback journal (not WAL) so search connections can
//! `ATTACH` it read-only without creating `-shm`/`-wal` files. Writes need
//! a [`WriteTx`] and are short: embeddings are computed before the lock is
//! taken.
//!
//! Like the index, the cache has an identity in `meta` (a random
//! `instance`, new whenever the cache is created or recreated, and a
//! `revision` every insert or prune moves): semantic and hybrid page
//! cursors ([`super::PageCursor`]) are bound to it.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior, params};

use super::{BUSY_TIMEOUT, db, schema};
use crate::{Error, Result, WriteTx, paths};

/// Relative to the library root.
pub const VECTORS_DB: &str = ".index/vectors.sqlite";

/// Version of the cache layout; another version is dropped and recreated.
pub(super) const CACHE_SCHEMA_VERSION: u32 = 1;

const DDL: &str = "
CREATE TABLE IF NOT EXISTS meta(
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS vectors(
    model TEXT NOT NULL,
    text_sha256 TEXT NOT NULL,
    dim INTEGER NOT NULL,
    vector BLOB NOT NULL,
    PRIMARY KEY(model, text_sha256)
) WITHOUT ROWID;
";

/// A connection to the library's vector cache.
pub struct VectorCache {
    conn: Connection,
}

impl VectorCache {
    /// Path of the cache inside the library at `lib_root`.
    pub fn path(lib_root: &Path) -> PathBuf {
        lib_root.join(VECTORS_DB)
    }

    /// Opens the cache, creating `.index/`, the file and its tables when
    /// missing. A cache of another layout version is emptied, and a file
    /// that is not a readable database is deleted and recreated (it is
    /// derived data).
    pub fn open(lib_root: &Path) -> Result<Self> {
        let dir = lib_root.join(paths::INDEX_DIR);
        fs::create_dir_all(&dir).map_err(|e| Error::io_at(&dir, e))?;
        let path = Self::path(lib_root);
        match open_conn(&path) {
            Ok(conn) => Ok(Self { conn }),
            Err(e)
                if matches!(
                    e.sqlite_error_code(),
                    Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt)
                ) =>
            {
                for p in [path.clone(), path.with_extension("sqlite-journal")] {
                    match fs::remove_file(&p) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(Error::io_at(&p, e)),
                    }
                }
                open_conn(&path).map(|conn| Self { conn }).map_err(db)
            }
            Err(e) => Err(db(e)),
        }
    }

    /// Which of `hashes` have a vector for `model`.
    pub fn present<'a>(
        &self,
        model: &str,
        hashes: impl IntoIterator<Item = &'a str>,
    ) -> Result<HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT 1 FROM vectors WHERE model = ?1 AND text_sha256 = ?2")
            .map_err(db)?;
        let mut out = HashSet::new();
        for h in hashes {
            if stmt.exists(params![model, h]).map_err(db)? {
                out.insert(h.to_owned());
            }
        }
        Ok(out)
    }

    /// The stored vector, if any.
    pub fn get(&self, model: &str, text_sha256: &str) -> Result<Option<Vec<f32>>> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT vector FROM vectors WHERE model = ?1 AND text_sha256 = ?2",
                params![model, text_sha256],
                |r| r.get(0),
            )
            .optional()
            .map_err(db)?;
        blob.map(|b| decode_vector(&b)).transpose()
    }

    /// Stores `(text hash, vector)` pairs for `model` in one transaction,
    /// L2-normalising each vector; existing entries are replaced. Empty or
    /// non-finite vectors are refused.
    pub fn insert(
        &mut self,
        _tx: &WriteTx<'_>,
        model: &str,
        entries: &[(String, Vec<f32>)],
    ) -> Result<usize> {
        if model.is_empty() {
            return Err(Error::InvalidInput("empty text-model key".into()));
        }
        let t = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        {
            let mut stmt = t
                .prepare_cached(
                    "INSERT OR REPLACE INTO vectors(model, text_sha256, dim, vector)
                     VALUES (?1, ?2, ?3, ?4)",
                )
                .map_err(db)?;
            for (hash, v) in entries {
                if v.is_empty() || v.iter().any(|x| !x.is_finite()) {
                    return Err(Error::Inference(format!(
                        "text model {model} returned an empty or non-finite vector"
                    )));
                }
                let mut v = v.clone();
                normalize(&mut v);
                stmt.execute(params![
                    model,
                    hash,
                    i64::try_from(v.len()).unwrap_or(i64::MAX),
                    encode_vector(&v)
                ])
                .map_err(db)?;
            }
        }
        schema::bump_revision(&t)?;
        t.commit().map_err(db)?;
        Ok(entries.len())
    }

    /// Deletes every entry (any model) whose text hash is not in
    /// `referenced` (the hashes of the index's windows); returns the count.
    pub fn prune(&mut self, _tx: &WriteTx<'_>, referenced: &HashSet<String>) -> Result<usize> {
        let t = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        let keys: Vec<(String, String)> = {
            let mut stmt = t
                .prepare("SELECT model, text_sha256 FROM vectors")
                .map_err(db)?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(db)?
                .collect::<rusqlite::Result<_>>()
                .map_err(db)?
        };
        let mut removed = 0;
        {
            let mut del = t
                .prepare("DELETE FROM vectors WHERE model = ?1 AND text_sha256 = ?2")
                .map_err(db)?;
            for (model, hash) in keys.iter().filter(|(_, h)| !referenced.contains(h)) {
                removed += del.execute(params![model, hash]).map_err(db)?;
            }
        }
        schema::bump_revision(&t)?;
        t.commit().map_err(db)?;
        Ok(removed)
    }

    /// Number of entries for `model`, or for every model with `None`.
    pub fn count(&self, model: Option<&str>) -> Result<usize> {
        let n: i64 = match model {
            Some(m) => {
                self.conn
                    .query_row("SELECT count(*) FROM vectors WHERE model = ?1", [m], |r| {
                        r.get(0)
                    })
            }
            None => self
                .conn
                .query_row("SELECT count(*) FROM vectors", [], |r| r.get(0)),
        }
        .map_err(db)?;
        Ok(usize::try_from(n).unwrap_or(0))
    }
}

/// Little-endian `f32`s.
pub fn encode_vector(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Inverse of [`encode_vector`].
pub fn decode_vector(b: &[u8]) -> Result<Vec<f32>> {
    if !b.len().is_multiple_of(4) {
        return Err(Error::Schema(format!(
            "vector blob of {} bytes is not a list of f32",
            b.len()
        )));
    }
    Ok(b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// Scales `v` to unit length (left as is when all zeros).
pub fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

/// Dot product of the stored (unit) blob with the unit query `q`; `None`
/// when the dimensions differ.
pub(super) fn dot_blob(blob: &[u8], q: &[f32]) -> Option<f32> {
    if blob.len() != q.len() * 4 {
        return None;
    }
    Some(
        blob.as_chunks::<4>()
            .0
            .iter()
            .zip(q)
            .map(|(c, y)| f32::from_le_bytes([c[0], c[1], c[2], c[3]]) * y)
            .sum(),
    )
}

/// `file:` URI of `path` opened read-only, for `ATTACH`.
pub(super) fn readonly_uri(path: &Path) -> String {
    let mut uri = String::from("file:");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~') {
            uri.push(char::from(b));
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri.push_str("?mode=ro");
    uri
}

/// Opens (creating) the cache database and brings its layout up to date.
fn open_conn(path: &Path) -> rusqlite::Result<Connection> {
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.query_row("PRAGMA journal_mode = DELETE", [], |r| {
        r.get::<_, String>(0)
    })?;
    let t = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    t.execute_batch(DDL)?;
    let version: Option<String> = t
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if version.as_deref() != Some(CACHE_SCHEMA_VERSION.to_string().as_str()) {
        t.execute_batch("DROP TABLE vectors")?;
        t.execute_batch(DDL)?;
        t.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES ('schema_version', ?1)",
            [CACHE_SCHEMA_VERSION.to_string()],
        )?;
        t.execute("DELETE FROM meta WHERE key IN ('instance', 'revision')", [])?;
    }
    t.execute(
        "INSERT OR IGNORE INTO meta(key, value) VALUES ('instance', ?1)",
        [format!("{:032x}", rand::random::<u128>())],
    )?;
    t.commit()?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Library;

    #[test]
    fn insert_present_get_and_prune() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let mut cache = VectorCache::open(lib.root()).unwrap();
        lib.write(|tx| {
            cache.insert(
                tx,
                "m",
                &[("a".into(), vec![3.0, 4.0]), ("b".into(), vec![1.0, 0.0])],
            )?;
            cache.insert(tx, "other", &[("a".into(), vec![0.0, 2.0])])
        })
        .unwrap();
        let present = cache.present("m", ["a", "c"]).unwrap();
        assert_eq!(present, HashSet::from(["a".to_owned()]));
        assert_eq!(cache.get("m", "a").unwrap(), Some(vec![0.6, 0.8]));
        assert_eq!(cache.count(None).unwrap(), 3);

        let referenced = HashSet::from(["b".to_owned()]);
        let removed = lib.write(|tx| cache.prune(tx, &referenced)).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(cache.count(Some("m")).unwrap(), 1);
        assert_eq!(cache.get("other", "a").unwrap(), None);

        let err = lib
            .write(|tx| cache.insert(tx, "m", &[("x".into(), vec![f32::NAN])]))
            .unwrap_err();
        assert_eq!(err.code(), "inference");
        // The failed batch left nothing behind.
        assert_eq!(cache.count(Some("m")).unwrap(), 1);
        drop(cache);
        // Survives reopening.
        assert_eq!(
            VectorCache::open(lib.root()).unwrap().count(None).unwrap(),
            1
        );
    }

    #[test]
    fn uri_escapes_special_characters() {
        assert_eq!(
            readonly_uri(Path::new("/a b/c?#%.sqlite")),
            "file:/a%20b/c%3F%23%25.sqlite?mode=ro"
        );
    }
}
