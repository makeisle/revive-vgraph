//! SQLite 백엔드 (계획 D6: rusqlite bundled, 그래프당 파일 1개, WAL 모드).

use crate::api::*;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use smol_str::SmolStr;
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use vgraph_core::{Hash256, Uuid, WorkingId};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS blobs      ( hash TEXT PRIMARY KEY, bytes BLOB NOT NULL );
CREATE TABLE IF NOT EXISTS refs       ( branch TEXT PRIMARY KEY, head TEXT NOT NULL );
CREATE TABLE IF NOT EXISTS payloads   ( hash TEXT PRIMARY KEY, props TEXT NOT NULL );
CREATE TABLE IF NOT EXISTS link_nodes ( branch TEXT NOT NULL, uuid TEXT NOT NULL,
    labels TEXT NOT NULL, slug TEXT, payload TEXT NOT NULL, PRIMARY KEY(branch, uuid) );
CREATE TABLE IF NOT EXISTS link_edges ( branch TEXT NOT NULL, uuid TEXT NOT NULL,
    etype TEXT NOT NULL, from_uuid TEXT NOT NULL, to_uuid TEXT NOT NULL,
    payload TEXT NOT NULL, PRIMARY KEY(branch, uuid) );
CREATE TABLE IF NOT EXISTS link_adj   ( branch TEXT NOT NULL, uuid TEXT NOT NULL,
    adj BLOB NOT NULL, PRIMARY KEY(branch, uuid) );
CREATE TABLE IF NOT EXISTS watermarks ( branch TEXT PRIMARY KEY, applied_commit TEXT NOT NULL );
CREATE TABLE IF NOT EXISTS working    ( temp_id TEXT NOT NULL, seq INTEGER NOT NULL,
    owner TEXT NOT NULL, op TEXT NOT NULL, PRIMARY KEY(temp_id, seq) );
"#;

pub struct SqliteBackend {
    conn: Mutex<Connection>,
    subs: Mutex<Vec<Sender<RefEvent>>>,
}

impl SqliteBackend {
    pub fn open(path: &Path) -> StoreResult<Self> {
        let conn = Connection::open(path)?;
        // WAL 모드 (반환 row 소비)
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute_batch(SCHEMA)?;
        Ok(SqliteBackend {
            conn: Mutex::new(conn),
            subs: Mutex::new(Vec::new()),
        })
    }

    fn notify(&self, ev: RefEvent) {
        self.subs
            .lock()
            .unwrap()
            .retain(|s| s.send(ev.clone()).is_ok());
    }
}

fn parse_hash(s: &str) -> StoreResult<Hash256> {
    s.parse()
        .map_err(|e| StoreError::Backend(format!("bad hash in db: {e}")))
}

fn parse_uuid(s: &str) -> StoreResult<Uuid> {
    s.parse()
        .map_err(|e| StoreError::Backend(format!("bad uuid in db: {e:?}")))
}

fn labels_to_json(labels: &[SmolStr]) -> String {
    serde_json::to_string(&labels.iter().map(|l| l.as_str()).collect::<Vec<_>>())
        .expect("labels json")
}

fn labels_from_json(s: &str) -> StoreResult<Vec<SmolStr>> {
    let v: Vec<String> = serde_json::from_str(s).map_err(|e| StoreError::Backend(e.to_string()))?;
    Ok(v.into_iter().map(SmolStr::from).collect())
}

impl BlobStore for SqliteBackend {
    fn put_blob(&self, hash: &Hash256, bytes: &[u8]) -> StoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO blobs(hash, bytes) VALUES(?1, ?2)",
            params![hash.to_string(), bytes],
        )?;
        Ok(())
    }

    fn get_blob(&self, hash: &Hash256) -> StoreResult<Option<Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT bytes FROM blobs WHERE hash=?1",
                params![hash.to_string()],
                |r| r.get(0),
            )
            .optional()?)
    }

    fn has_blob(&self, hash: &Hash256) -> StoreResult<bool> {
        Ok(self.get_blob(hash)?.is_some())
    }

    fn list_blobs(&self) -> StoreResult<Vec<Hash256>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT hash FROM blobs ORDER BY hash")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(parse_hash(&row?)?);
        }
        Ok(out)
    }

    fn delete_blob(&self, hash: &Hash256) -> StoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM blobs WHERE hash=?1", params![hash.to_string()])?;
        Ok(())
    }
}

impl RefStore for SqliteBackend {
    fn cas_ref(
        &self,
        branch: &str,
        expect: Option<&Hash256>,
        new: &Hash256,
    ) -> StoreResult<CasOutcome> {
        let outcome = {
            let mut conn = self.conn.lock().unwrap();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let cur: Option<String> = tx
                .query_row(
                    "SELECT head FROM refs WHERE branch=?1",
                    params![branch],
                    |r| r.get(0),
                )
                .optional()?;
            let cur_h = match &cur {
                Some(s) => Some(parse_hash(s)?),
                None => None,
            };
            if cur_h.as_ref() != expect {
                return Ok(CasOutcome::Conflict { actual: cur_h });
            }
            tx.execute(
                "INSERT INTO refs(branch, head) VALUES(?1, ?2)
                 ON CONFLICT(branch) DO UPDATE SET head=?2",
                params![branch, new.to_string()],
            )?;
            tx.commit()?;
            RefEvent {
                branch: branch.to_string(),
                from: cur_h,
                to: *new,
            }
        };
        self.notify(outcome);
        Ok(CasOutcome::Committed)
    }

    fn get_ref(&self, branch: &str) -> StoreResult<Option<Hash256>> {
        let conn = self.conn.lock().unwrap();
        let cur: Option<String> = conn
            .query_row(
                "SELECT head FROM refs WHERE branch=?1",
                params![branch],
                |r| r.get(0),
            )
            .optional()?;
        cur.as_deref().map(parse_hash).transpose()
    }

    fn list_refs(&self) -> StoreResult<Vec<(String, Hash256)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT branch, head FROM refs ORDER BY branch")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (b, h) = row?;
            out.push((b, parse_hash(&h)?));
        }
        Ok(out)
    }
}

impl PayloadStore for SqliteBackend {
    fn put_payload(&self, hash: &Hash256, props_json: &str) -> StoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO payloads(hash, props) VALUES(?1, ?2)",
            params![hash.to_string(), props_json],
        )?;
        Ok(())
    }

    fn get_payload(&self, hash: &Hash256) -> StoreResult<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT props FROM payloads WHERE hash=?1",
                params![hash.to_string()],
                |r| r.get(0),
            )
            .optional()?)
    }

    fn scan_index(&self, key: &str, value_json: &str) -> StoreResult<Vec<Hash256>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT hash FROM payloads
             WHERE json_extract(props, '$.' || ?1) = json_extract(?2, '$')
             ORDER BY hash",
        )?;
        let rows = stmt.query_map(params![key, value_json], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(parse_hash(&row?)?);
        }
        Ok(out)
    }

    fn list_payloads(&self) -> StoreResult<Vec<Hash256>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT hash FROM payloads ORDER BY hash")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(parse_hash(&row?)?);
        }
        Ok(out)
    }

    fn delete_payload(&self, hash: &Hash256) -> StoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM payloads WHERE hash=?1",
            params![hash.to_string()],
        )?;
        Ok(())
    }
}

impl HeadProvision for SqliteBackend {
    fn upsert_head(&self, branch: &str, rows: &LinkRows) -> StoreResult<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for n in &rows.upsert_nodes {
            tx.execute(
                "INSERT INTO link_nodes(branch, uuid, labels, slug, payload)
                 VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(branch, uuid) DO UPDATE SET labels=?3, slug=?4, payload=?5",
                params![
                    branch,
                    n.uuid.to_string(),
                    labels_to_json(&n.labels),
                    n.slug.as_deref(),
                    n.payload.to_string()
                ],
            )?;
        }
        for u in &rows.remove_nodes {
            tx.execute(
                "DELETE FROM link_nodes WHERE branch=?1 AND uuid=?2",
                params![branch, u.to_string()],
            )?;
        }
        for e in &rows.upsert_edges {
            tx.execute(
                "INSERT INTO link_edges(branch, uuid, etype, from_uuid, to_uuid, payload)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(branch, uuid) DO UPDATE SET etype=?3, from_uuid=?4, to_uuid=?5, payload=?6",
                params![
                    branch,
                    e.uuid.to_string(),
                    e.etype.as_str(),
                    e.from.to_string(),
                    e.to.to_string(),
                    e.payload.to_string()
                ],
            )?;
        }
        for u in &rows.remove_edges {
            tx.execute(
                "DELETE FROM link_edges WHERE branch=?1 AND uuid=?2",
                params![branch, u.to_string()],
            )?;
        }
        for (u, adj) in &rows.adjacency {
            tx.execute(
                "INSERT INTO link_adj(branch, uuid, adj) VALUES(?1, ?2, ?3)
                 ON CONFLICT(branch, uuid) DO UPDATE SET adj=?3",
                params![branch, u.to_string(), adj],
            )?;
        }
        for u in &rows.remove_adjacency {
            tx.execute(
                "DELETE FROM link_adj WHERE branch=?1 AND uuid=?2",
                params![branch, u.to_string()],
            )?;
        }
        tx.execute(
            "INSERT INTO watermarks(branch, applied_commit) VALUES(?1, ?2)
             ON CONFLICT(branch) DO UPDATE SET applied_commit=?2",
            params![branch, rows.watermark.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn scan_nodes(&self, branch: &str) -> StoreResult<Vec<NodeRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT uuid, labels, slug, payload FROM link_nodes WHERE branch=?1 ORDER BY uuid",
        )?;
        let rows = stmt.query_map(params![branch], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (uuid, labels, slug, payload) = row?;
            out.push(NodeRow {
                uuid: parse_uuid(&uuid)?,
                labels: labels_from_json(&labels)?,
                slug: slug.map(SmolStr::from),
                payload: parse_hash(&payload)?,
            });
        }
        Ok(out)
    }

    fn scan_edges(&self, branch: &str) -> StoreResult<Vec<EdgeRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT uuid, etype, from_uuid, to_uuid, payload FROM link_edges
             WHERE branch=?1 ORDER BY uuid",
        )?;
        let rows = stmt.query_map(params![branch], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (uuid, etype, from, to, payload) = row?;
            out.push(EdgeRow {
                uuid: parse_uuid(&uuid)?,
                etype: SmolStr::from(etype),
                from: parse_uuid(&from)?,
                to: parse_uuid(&to)?,
                payload: parse_hash(&payload)?,
            });
        }
        Ok(out)
    }

    fn adjacency(&self, branch: &str, uuid: Uuid) -> StoreResult<Option<Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT adj FROM link_adj WHERE branch=?1 AND uuid=?2",
                params![branch, uuid.to_string()],
                |r| r.get(0),
            )
            .optional()?)
    }

    fn watermark(&self, branch: &str) -> StoreResult<Option<Hash256>> {
        let conn = self.conn.lock().unwrap();
        let s: Option<String> = conn
            .query_row(
                "SELECT applied_commit FROM watermarks WHERE branch=?1",
                params![branch],
                |r| r.get(0),
            )
            .optional()?;
        s.as_deref().map(parse_hash).transpose()
    }

    fn set_watermark(&self, branch: &str, commit: &Hash256) -> StoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO watermarks(branch, applied_commit) VALUES(?1, ?2)
             ON CONFLICT(branch) DO UPDATE SET applied_commit=?2",
            params![branch, commit.to_string()],
        )?;
        Ok(())
    }

    fn reset_derived(&self) -> StoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "DELETE FROM link_nodes; DELETE FROM link_edges;
             DELETE FROM link_adj; DELETE FROM watermarks;",
        )?;
        Ok(())
    }
}

impl WalStore for SqliteBackend {
    fn append_wal(&self, w: WorkingId, owner: &str, op_json: &str) -> StoreResult<u64> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let seq: u64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM working WHERE temp_id=?1",
            params![w.to_string()],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO working(temp_id, seq, owner, op) VALUES(?1, ?2, ?3, ?4)",
            params![w.to_string(), seq, owner, op_json],
        )?;
        tx.commit()?;
        Ok(seq)
    }

    fn read_wal(&self, w: WorkingId) -> StoreResult<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT op FROM working WHERE temp_id=?1 ORDER BY seq")?;
        let rows = stmt.query_map(params![w.to_string()], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn list_wals(&self, owner: &str) -> StoreResult<Vec<WorkingId>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT DISTINCT temp_id FROM working WHERE owner=?1 ORDER BY temp_id")?;
        let rows = stmt.query_map(params![owner], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let s = row?;
            out.push(
                s.parse::<WorkingId>()
                    .map_err(|e| StoreError::Backend(format!("bad working id: {e:?}")))?,
            );
        }
        Ok(out)
    }

    fn drop_wal(&self, w: WorkingId) -> StoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM working WHERE temp_id=?1",
            params![w.to_string()],
        )?;
        Ok(())
    }
}

impl RefWatch for SqliteBackend {
    fn subscribe(&self) -> Receiver<RefEvent> {
        let (tx, rx) = channel();
        self.subs.lock().unwrap().push(tx);
        rx
    }
}

impl StorageBackend for SqliteBackend {
    fn stats(&self) -> StoreResult<StorageStats> {
        let conn = self.conn.lock().unwrap();
        let blobs: u64 = conn.query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))?;
        let payload_rows: u64 =
            conn.query_row("SELECT COUNT(*) FROM payloads", [], |r| r.get(0))?;
        Ok(StorageStats {
            blobs,
            payload_rows,
        })
    }
}
