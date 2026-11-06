//! 存储层：SQLite(WAL) 存元数据 + blake3 内容寻址的外部 blob。
//!
//! 设计要点：
//! - 只存元数据、一个短预览，外加长文本的正文尾部（`tail`，专给检索用）。
//!   SQLite 页永远不会被大内容撑爆。
//! - 超过 `INLINE_LIMIT` 的内容落到 `blobs/xx/<hash>`，文件名就是内容哈希，
//!   天然去重，也不需要额外的 GC 索引。
//! - **不在 SQLite 里做全文检索**。检索交给 `crate::search::Index`：启动时把
//!   全量元数据装进内存，之后每次查询都是纯内存子串扫描。理由见那个模块的
//!   文档——简单说，1 万条 × 512 字的内存扫描比 FTS5 trigram 快一个数量级，
//!   而且没有「少于 3 个字符就搜不到」的分词器限制。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::model::{Captured, INLINE_LIMIT, Kind, PREVIEW_CHARS, TAIL_CHARS};

/// 库结构版本，存进 `PRAGMA user_version`。
///
/// 有了它，「这次启动要不要补数据」才有一个确定的判据。只靠
/// `WHERE tail = ''` 判断的话，每次启动都会把那些「短但字节多」的汉字条目
/// 重新读一遍 payload，白花时间。
const SCHEMA_VERSION: i64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    /// 全新条目。
    New(i64),
    /// 已存在，只更新了时间戳和命中次数。
    Bumped(i64),
}

#[derive(Debug, Clone)]
pub struct EntryMeta {
    pub id: i64,
    pub kind: Kind,
    pub preview: String,
    pub byte_len: i64,
    pub created_at: i64,
    pub hit_count: i64,
    /// 收藏的条目不会被容量淘汰。值本身不参与排序——列表始终按时间倒序，
    /// 收藏项就留在它本来该在的位置上，翻起来才有可预测的顺序。
    pub pinned: bool,
}

/// 历史条目上限。超过就按最旧优先淘汰。
///
/// 剪贴板工具是常驻的，不设上限迟早把磁盘吃满——尤其是图片，
/// 几次 4K 截图就是几十 MB。
const MAX_ENTRIES: i64 = 10_000;

pub struct Store {
    conn: Connection,
    blob_dir: PathBuf,
}

/// UI 线程用的只读视图。
pub struct ReadStore {
    conn: Connection,
}

impl ReadStore {
    pub fn recent(&self, limit: usize) -> Result<Vec<EntryMeta>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, kind, preview, byte_len, created_at, hit_count, pinned
             FROM clips ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(EntryMeta {
                id: r.get(0)?,
                kind: Kind::from_i64(r.get(1)?).unwrap_or(Kind::Text),
                preview: r.get(2)?,
                byte_len: r.get(3)?,
                created_at: r.get(4)?,
                hit_count: r.get(5)?,
                pinned: r.get::<_, i64>(6)? != 0,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn total(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM clips", [], |r| r.get(0))?)
    }

    pub fn load_payload(&self, id: i64) -> Result<Option<Vec<u8>>> {
        let row: Option<(Option<Vec<u8>>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT inline, blob_path FROM clips WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        let Some((inline, blob_path)) = row else {
            return Ok(None);
        };

        if let Some(data) = inline {
            return Ok(Some(data));
        }
        if let Some(rel) = blob_path {
            // 只读连接需要 blob 目录的绝对路径才能加载内容。
            let path = std::path::Path::new(&self.blob_root()).join(rel);
            return Ok(Some(std::fs::read(path)?));
        }
        Ok(None)
    }

    fn blob_root(&self) -> std::path::PathBuf {
        let db = self.conn.path().expect("数据库是文件");
        std::path::Path::new(db).parent().unwrap().join("blobs")
    }

    /// 全量元数据，按时间倒序。
    ///
    /// 只在两种时机调用：启动时灌满内存索引、索引和库对不上时全量校正。
    /// 1 万条约 10–20 ms，不放在搜索路径上。
    pub fn all_meta(&self) -> Result<Vec<EntryMeta>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, kind, preview, byte_len, created_at, hit_count, pinned
             FROM clips ORDER BY created_at DESC",
        )?;
        collect_meta(&mut stmt, &[])
    }

    /// 全量正文尾部 `(id, tail)`。只返回真有尾部的行（长文本），
    /// 短条目和图片/文件不在结果里——它们占绝大多数，没必要传一遍空串。
    pub fn all_tails(&self) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, tail FROM clips WHERE tail <> ''")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 增量拉正文尾部，语义和 `meta_since` 一致（id 大于 `after_id`）。
    pub fn tails_since(&self, after_id: i64) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, tail FROM clips WHERE id > ?1 AND tail <> ''")?;
        let rows = stmt.query_map(params![after_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 增量同步：取 id 大于 `after_id` 的条目。
    ///
    /// 新条目 id 一定更大（AUTOINCREMENT 语义 + 单写入者），所以这比
    /// 「比 created_at」可靠——改时间戳的 bump 不影响 id。
    pub fn meta_since(&self, after_id: i64) -> Result<Vec<EntryMeta>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, kind, preview, byte_len, created_at, hit_count, pinned
             FROM clips WHERE id > ?1 ORDER BY created_at DESC",
        )?;
        collect_meta(&mut stmt, params![after_id])
    }
}

/// 把 `clips` 的行映射成 `EntryMeta`。列顺序固定为
/// `id, kind, preview, byte_len, created_at, hit_count, pinned`。
fn collect_meta(
    stmt: &mut rusqlite::Statement<'_>,
    p: &[&dyn rusqlite::ToSql],
) -> Result<Vec<EntryMeta>> {
    let mut rows = stmt.query(p)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(EntryMeta {
            id: row.get(0)?,
            kind: Kind::from_i64(row.get(1)?).unwrap_or(Kind::Text),
            preview: row.get(2)?,
            byte_len: row.get(3)?,
            created_at: row.get(4)?,
            hit_count: row.get(5)?,
            pinned: row.get::<_, i64>(6)? != 0,
        });
    }
    Ok(out)
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("无法创建数据目录 {}", root.display()))?;
        let blob_dir = root.join("blobs");
        std::fs::create_dir_all(&blob_dir)?;

        let conn = Connection::open(root.join("rpaste.db"))?;

        // WAL + NORMAL：单个写入者的场景下吞吐最高，且崩溃只会丢最后一两个事务。
        //
        // `wal_autocheckpoint` 默认 1000 页（约 4MB）才合并一次，剪贴板这种
        // 低频写入的场景可能几天都不触发——主库文件会一直停在打开时的旧快照上，
        // 数据全在 `-wal` 里。这会让「拷一份 .db 当备份」变成拷一个空库。
        // 降到 256 页（约 1MB），让主库文件在运行期也能保持接近最新。
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA temp_store = MEMORY;
             PRAGMA cache_size = -8192;
             PRAGMA busy_timeout = 2000;
             PRAGMA wal_autocheckpoint = 256;",
        )?;

        let store = Self { conn, blob_dir };
        store.migrate()?;

        // 启动时补一次合并。
        //
        // `wal_checkpoint` 原本只在收到 `Shutdown` 时调用一次；进程被强杀、
        // 崩溃、或者反复重启（每次都只看 WAL 不看主库）时，WAL 里的旧帧
        // 永远没机会折回主库。实测一个只跑过几十次的库，-wal 攒到 3.4 MB，
        // 而主库停在 148 KB —— 「拷一份 .db 当备份」会拿到几乎空的库。
        //
        // 用 TRUNCATE 而不是 PASSIVE：PASSIVE 只是把帧写回主库、WAL 文件
        // 大小不变，磁盘上那几 MB 依然占着。启动这一刻我们是唯一的连接
        // （只读连接在稍后才建），TRUNCATE 能安全地真的缩小文件。
        // 失败也不影响使用，下次退出时还会再合一次。
        if let Err(err) = store.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
            crate::log_warn!("启动时合并 WAL 失败（不影响使用）: {err}");
        }

        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        // 老版本用 FTS5 trigram 做检索，现在改成内存索引，索引表没必要留：
        // 它占了 1/3 的库体积，触发器还让每次写入多一次索引维护。
        // 老库里若存在就顺手删掉（历史数据不必保留）。
        if self.has_table("clips_fts")? {
            self.conn.execute_batch(
                "DROP TRIGGER IF EXISTS clips_ai;
                 DROP TRIGGER IF EXISTS clips_ad;
                 DROP TRIGGER IF EXISTS clips_au;
                 DROP TABLE IF EXISTS clips_fts;",
            )?;
            crate::log_info!("已移除旧的 FTS5 索引表，检索改用内存索引");
        }

        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS clips (
                 id         INTEGER PRIMARY KEY,
                 kind       INTEGER NOT NULL,
                 hash       BLOB    NOT NULL,
                 preview    TEXT    NOT NULL,
                 tail       TEXT    NOT NULL DEFAULT '',
                 byte_len   INTEGER NOT NULL,
                 inline     BLOB,
                 blob_path  TEXT,
                 created_at INTEGER NOT NULL,
                 hit_count  INTEGER NOT NULL DEFAULT 0
             );
             CREATE UNIQUE INDEX IF NOT EXISTS idx_clips_hash ON clips(hash);
             CREATE INDEX IF NOT EXISTS idx_clips_recent ON clips(created_at DESC);",
        )?;

        // 老库升级：`pinned` 是后加的列，CREATE TABLE IF NOT EXISTS 不会补列。
        // 不判断就直接 ALTER 会在新库上报「duplicate column name」。
        if !self.has_column("clips", "pinned")? {
            self.conn
                .execute_batch("ALTER TABLE clips ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;")?;
        }

        // `tail`（v2）：预览之外的正文尾部，专给检索用。
        // 以前只有 512 字预览，长文本的后半段搜不到——复制过的东西搜不出来
        // 是最伤信任的一类缺陷，宁可多存一份尾部。
        if !self.has_column("clips", "tail")? {
            self.conn
                .execute_batch("ALTER TABLE clips ADD COLUMN tail TEXT NOT NULL DEFAULT '';")?;
        }
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < SCHEMA_VERSION {
            self.backfill_tails()?;
            self.conn
                .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        }

        // 淘汰时要按「未收藏优先、越旧越先」找牺牲者，让这个排序走索引。
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_clips_evict ON clips(pinned ASC, created_at ASC);",
        )?;

        Ok(())
    }

    fn has_table(&self, name: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    fn has_column(&self, table: &str, column: &str) -> Result<bool> {
        // PRAGMA 不支持参数绑定，这里的表名是代码里的常量，没有注入面。
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            if row.get::<_, String>(1)? == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 给老数据补上检索用的正文尾部，一次性动作（`user_version` 记住做过没有）。
    ///
    /// 只处理文本类、且字节数超过预览上限的行——只有它们可能还有尾部。
    /// 超过 `INLINE_LIMIT` 的正文在 blob 文件里，也一并读出来，否则用户手里
    /// 那几个大条目升级后依然搜不到后半段，看起来就像「修了但没修」。
    fn backfill_tails(&self) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT id, inline, blob_path FROM clips
             WHERE tail = '' AND kind IN (?1, ?2) AND byte_len > ?3",
        )?;
        let rows: Vec<(i64, Option<Vec<u8>>, Option<String>)> = stmt
            .query_map(
                params![
                    Kind::Text.as_i64(),
                    Kind::RichText.as_i64(),
                    PREVIEW_CHARS as i64
                ],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?
            .filter_map(|row| row.ok())
            .collect();
        drop(stmt);

        let mut updated = 0usize;
        for (id, inline, blob_path) in rows {
            let data = match (inline, blob_path) {
                (Some(data), _) => data,
                (None, Some(rel)) => match std::fs::read(self.blob_dir.join(rel)) {
                    Ok(data) => data,
                    // 文件被手工删了：跳过，别为一条坏数据挡住整次升级。
                    Err(_) => continue,
                },
                (None, None) => continue,
            };
            let text = String::from_utf8_lossy(&data);
            let (_, tail) = crate::model::make_preview_and_tail(&text, PREVIEW_CHARS, TAIL_CHARS);
            if tail.is_empty() {
                continue;
            }
            self.conn.execute(
                "UPDATE clips SET tail = ?1 WHERE id = ?2",
                params![tail, id],
            )?;
            updated += 1;
        }

        if updated > 0 {
            crate::log_info!("已为 {updated} 条长文本补上可检索正文");
        }
        Ok(())
    }

    pub fn insert(&mut self, c: &Captured) -> Result<InsertOutcome> {
        let now = now_ms();

        // 先查后写：单写入者场景下没有竞态，比 ON CONFLICT 更省一次 blob 落盘。
        if let Some(id) = self.find_by_hash(&c.hash)? {
            self.conn.execute(
                "UPDATE clips SET created_at = ?1, hit_count = hit_count + 1 WHERE id = ?2",
                params![now, id],
            )?;
            return Ok(InsertOutcome::Bumped(id));
        }

        let (inline, blob_path) = if c.payload.len() <= INLINE_LIMIT {
            (Some(c.payload.as_slice()), None)
        } else {
            (None, Some(self.write_blob(&c.hash, &c.payload)?))
        };

        self.conn.execute(
            "INSERT INTO clips (kind, hash, preview, tail, byte_len, inline, blob_path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                c.kind.as_i64(),
                &c.hash[..],
                c.preview,
                c.tail,
                c.payload.len() as i64,
                inline,
                blob_path,
                now
            ],
        )?;

        // 只有真正新增才可能超限；去重只改时间戳，总数不变。
        self.enforce_limit(MAX_ENTRIES)?;

        Ok(InsertOutcome::New(self.conn.last_insert_rowid()))
    }

    /// 超出上限时淘汰，同时删掉对应的 blob 文件。
    ///
    /// 淘汰顺序：**未收藏的优先，其中越旧越先**。收藏项只有在一个都不剩时
    /// 才会被动到——如果用户把所有条目都收藏了，这里会放弃淘汰、宁可超限。
    /// 收藏是明确意图，不该被容量规则静默推翻。
    ///
    /// blob 是内容寻址的，`hash` 唯一索引保证一条内容只对应一条记录，
    /// 所以删记录后文件可以安全删除，不会误删别的条目还在用的数据。
    pub(crate) fn enforce_limit(&mut self, max: i64) -> Result<usize> {
        let total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM clips", [], |r| r.get(0))?;
        if total <= max {
            return Ok(0);
        }
        let excess = total - max;

        // 先记下要删的 blob 路径，记录删掉之后就查不到了
        let mut stmt = self.conn.prepare(
            "SELECT id, blob_path FROM clips
             WHERE pinned = 0
             ORDER BY created_at ASC
             LIMIT ?1",
        )?;
        let doomed: Vec<(i64, Option<String>)> = stmt
            .query_map(params![excess], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|row| row.ok())
            .collect();
        drop(stmt);

        for (id, _) in &doomed {
            self.conn
                .execute("DELETE FROM clips WHERE id = ?1", params![id])?;
        }

        for (_, rel) in &doomed {
            if let Some(rel) = rel {
                // 删不掉也只是留个孤儿文件，不影响功能
                let _ = std::fs::remove_file(self.blob_dir.join(rel));
            }
        }

        Ok(doomed.len())
    }

    /// 把 WAL 里的事务合并回主库文件。退出前调用一次。
    ///
    /// 用 `FULL` 而不是 `TRUNCATE`：UI 线程还拿着只读连接，`TRUNCATE` 要求
    /// 独占、会失败。`FULL` 已经足以保证主库文件是完整的——这才是不让
    /// 「只拷 .db 当备份」变成拷空库的关键。
    pub fn checkpoint(&self) -> Result<()> {
        // 返回 (busy, WAL 里的帧数, 已合并帧数)。`busy = 1` 说明还有别的连接
        // 占着读事务，这次没合完——退出流程里必须能看见，否则备份又变成
        // 空库还没人知道。
        let (busy, log, done): (i64, i64, i64) =
            self.conn
                .query_row("PRAGMA wal_checkpoint(FULL)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
        if busy != 0 || done < log {
            crate::log_warn!("WAL 未完全合并: busy={busy} 帧 {log}，已合并 {done}");
        }
        Ok(())
    }

    /// 删除一条记录，连带它的 blob 文件。返回是否真的删掉了。
    pub fn delete(&mut self, id: i64) -> Result<bool> {
        let blob: Option<String> = self
            .conn
            .query_row(
                "SELECT blob_path FROM clips WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();

        let n = self
            .conn
            .execute("DELETE FROM clips WHERE id = ?1", params![id])?;
        if n == 0 {
            return Ok(false);
        }
        if let Some(rel) = blob {
            let _ = std::fs::remove_file(self.blob_dir.join(rel));
        }
        Ok(true)
    }

    /// 清空历史。收藏项默认保留——「清空」不该顺手把用户精心收藏的东西端掉。
    pub fn clear(&mut self, keep_pinned: bool) -> Result<usize> {
        let cond = if keep_pinned { "WHERE pinned = 0" } else { "" };

        let mut stmt = self
            .conn
            .prepare(&format!("SELECT blob_path FROM clips {cond}"))?;
        let doomed: Vec<String> = stmt
            .query_map([], |r| r.get::<_, Option<String>>(0))?
            .filter_map(|row| row.ok().flatten())
            .collect();
        drop(stmt);

        let n = self
            .conn
            .execute(&format!("DELETE FROM clips {cond}"), [])?;

        for rel in doomed {
            let _ = std::fs::remove_file(self.blob_dir.join(rel));
        }
        Ok(n)
    }

    /// 设置收藏标记。收藏项不会被容量淘汰。
    pub fn set_pinned(&mut self, id: i64, pinned: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE clips SET pinned = ?1 WHERE id = ?2",
            params![pinned as i64, id],
        )?;
        Ok(())
    }

    fn find_by_hash(&self, hash: &[u8; 32]) -> Result<Option<i64>> {
        let id = self
            .conn
            .query_row(
                "SELECT id FROM clips WHERE hash = ?1",
                params![&hash[..]],
                |r| r.get(0),
            )
            .optional()?;
        Ok(id)
    }

    /// 写入内容文件，返回相对 `blob_dir` 的路径。
    fn write_blob(&self, hash: &[u8; 32], data: &[u8]) -> Result<String> {
        let hex = to_hex(hash);
        let (sub, name) = hex.split_at(2);
        let dir = self.blob_dir.join(sub);
        let path = dir.join(name);

        if path.exists() {
            return Ok(format!("{sub}/{name}"));
        }

        std::fs::create_dir_all(&dir)?;
        // 先写临时文件再 rename，避免崩溃时留下半截文件被当成有效内容。
        let tmp = dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, &path)?;
        Ok(format!("{sub}/{name}"))
    }

    /// 供 UI 线程只读查询。worker 持有写连接，UI 持有独立读连接，
    /// SQLite 的 WAL 模式天然支持「一写多读」，互不阻塞。
    pub fn read_only(&self) -> Result<ReadStore> {
        let path = self
            .conn
            .path()
            .ok_or_else(|| anyhow::anyhow!("数据库不是文件"))?;
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.execute_batch("PRAGMA query_only = ON;")?;
        Ok(ReadStore { conn })
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Captured;

    fn temp_store() -> (Store, PathBuf) {
        // 同一个测试进程里 pid 相同，必须再加一个自增序号，
        // 否则并行跑的用例会互相删掉对方的库。
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rpaste-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (Store::open(&dir).unwrap(), dir)
    }

    #[test]
    fn insert_and_dedup() {
        let (mut store, dir) = temp_store();
        let read = store.read_only().unwrap();
        let c = Captured::new(Kind::Text, b"hello".to_vec(), "hello".into());

        assert!(matches!(store.insert(&c).unwrap(), InsertOutcome::New(_)));
        assert!(matches!(
            store.insert(&c).unwrap(),
            InsertOutcome::Bumped(_)
        ));
        assert_eq!(read.total().unwrap(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn large_payload_goes_to_blob() {
        let (mut store, dir) = temp_store();
        let read = store.read_only().unwrap();
        let big = vec![b'x'; INLINE_LIMIT + 1];
        let c = Captured::new(Kind::Text, big.clone(), "big".into());
        store.insert(&c).unwrap();

        let meta = read.recent(1).unwrap();
        let id = meta[0].id;
        assert_eq!(read.load_payload(id).unwrap().unwrap().len(), big.len());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enforce_limit_drops_oldest() {
        let (mut store, dir) = temp_store();
        let read = store.read_only().unwrap();

        // 逐条插入，中间等一下，保证 created_at 严格递增
        for i in 0..5 {
            let text = format!("entry-{i}");
            let c = Captured::new(Kind::Text, text.clone().into_bytes(), text);
            store.insert(&c).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        assert_eq!(read.total().unwrap(), 5);

        let removed = store.enforce_limit(3).unwrap();
        assert_eq!(removed, 2, "应淘汰 2 条");
        assert_eq!(read.total().unwrap(), 3);

        // 留下的是最新的三条
        let left = read.recent(10).unwrap();
        let previews: Vec<&str> = left.iter().map(|e| e.preview.as_str()).collect();
        assert_eq!(previews, vec!["entry-4", "entry-3", "entry-2"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enforce_limit_removes_blob_file() {
        let (mut store, dir) = temp_store();
        let read = store.read_only().unwrap();

        // 大内容走 blob 文件
        let big = vec![b'x'; INLINE_LIMIT + 1];
        store
            .insert(&Captured::new(Kind::Text, big, "big".into()))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        store
            .insert(&Captured::new(
                Kind::Text,
                b"small".to_vec(),
                "small".into(),
            ))
            .unwrap();

        let blob_dir = dir.join("blobs");
        let count_blobs = || {
            std::fs::read_dir(&blob_dir)
                .map(|rd| {
                    rd.flatten()
                        .flat_map(|e| std::fs::read_dir(e.path()).ok().into_iter().flatten())
                        .count()
                })
                .unwrap_or(0)
        };
        assert_eq!(count_blobs(), 1, "大内容应落成一个 blob 文件");

        // 限制为 1 条，那条大内容（最旧）应被淘汰
        store.enforce_limit(1).unwrap();
        assert_eq!(read.total().unwrap(), 1);
        assert_eq!(count_blobs(), 0, "淘汰后 blob 文件也该删掉");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // 检索相关的测试搬到了 `crate::search`——那里才是检索语义的实现地，
    // 存储层只负责「把行读成 EntryMeta」。

    fn blob_count(dir: &Path) -> usize {
        let mut n = 0;
        if let Ok(subs) = std::fs::read_dir(dir.join("blobs")) {
            for sub in subs.flatten() {
                if sub.path().is_dir() {
                    n += std::fs::read_dir(sub.path())
                        .map(|d| d.count())
                        .unwrap_or(0);
                }
            }
        }
        n
    }

    #[test]
    fn delete_removes_row_and_blob() {
        let (mut store, dir) = temp_store();
        let big = vec![b'z'; INLINE_LIMIT + 10];
        let InsertOutcome::New(id) = store
            .insert(&Captured::new(Kind::Text, big, "big".into()))
            .unwrap()
        else {
            panic!("应该是新条目");
        };
        assert_eq!(blob_count(&dir), 1, "超大内容应该落到 blob");

        assert!(store.delete(id).unwrap());

        let read = store.read_only().unwrap();
        assert_eq!(read.total().unwrap(), 0);
        // 只删记录不删文件的话，磁盘占用不会降——这是最容易漏的一步。
        assert_eq!(blob_count(&dir), 0, "blob 文件也要一起删掉");

        // 删不存在的 id 不算错误
        assert!(!store.delete(id).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_drops_row_from_meta() {
        let (mut store, dir) = temp_store();
        let InsertOutcome::New(id) = store
            .insert(&Captured::new(
                Kind::Text,
                "剪贴板工具的性能设计".as_bytes().to_vec(),
                "剪贴板工具的性能设计".into(),
            ))
            .unwrap()
        else {
            panic!("应该是新条目");
        };

        store.delete(id).unwrap();

        let read = store.read_only().unwrap();
        // 删掉的行不能还出现在元数据里——否则内存索引会留一条空壳，
        // 粘贴时 `load_payload` 拿不到内容，用户看到的是「点了没反应」。
        assert!(read.all_meta().unwrap().is_empty());
        assert_eq!(read.total().unwrap(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enforce_limit_protects_pinned() {
        let (mut store, dir) = temp_store();
        let mut ids = Vec::new();
        for i in 0..5 {
            let text = format!("内容{i}");
            let InsertOutcome::New(id) = store
                .insert(&Captured::new(Kind::Text, text.clone().into_bytes(), text))
                .unwrap()
            else {
                panic!("应该是新条目");
            };
            ids.push(id);
        }

        // 收藏最旧的那条，再压到 3 条：被淘汰的应该是未收藏里最旧的。
        store.set_pinned(ids[0], true).unwrap();
        store.enforce_limit(3).unwrap();

        let read = store.read_only().unwrap();
        let left = read.recent(10).unwrap();
        assert_eq!(left.len(), 3);
        assert!(
            left.iter().any(|e| e.id == ids[0]),
            "收藏项不该被容量规则淘汰"
        );
        assert!(
            !left.iter().any(|e| e.id == ids[1]),
            "未收藏的最旧项应先被淘汰"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_keeps_pinned() {
        let (mut store, dir) = temp_store();
        let mut ids = Vec::new();
        for i in 0..3 {
            let text = format!("条目{i}");
            let InsertOutcome::New(id) = store
                .insert(&Captured::new(Kind::Text, text.clone().into_bytes(), text))
                .unwrap()
            else {
                panic!("应该是新条目");
            };
            ids.push(id);
        }
        store.set_pinned(ids[1], true).unwrap();

        assert_eq!(store.clear(true).unwrap(), 2);

        let read = store.read_only().unwrap();
        let left = read.recent(10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, ids[1]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkpoint_folds_wal_into_main_file() {
        let (mut store, dir) = temp_store();
        for i in 0..30 {
            let t = format!("内容{i}");
            store
                .insert(&Captured::new(Kind::Text, t.clone().into_bytes(), t))
                .unwrap();
        }

        store.checkpoint().unwrap();

        // 只拷主库文件（不带 -wal / -shm），模拟「拷 .db 当备份」。
        // 不能只比文件大小：数据摊进已有页时大小压根不变，这个断言会假失败。
        // 真正的语义是「主库文件自己就装得下全部记录」。早先没做退出前
        // checkpoint 的时候，主文件停在建库时的大小、数据全在 -wal 里，
        // 这样拷出来的备份是空的——这个坑实际踩过。
        let backup = dir.join("backup.db");
        std::fs::copy(dir.join("rpaste.db"), &backup).unwrap();
        {
            let conn = rusqlite::Connection::open(&backup).unwrap();
            let n: i64 = conn
                .query_row("SELECT count(*) FROM clips", [], |row| row.get(0))
                .unwrap();
            assert_eq!(n, 30, "checkpoint 后主库文件本身应包含全部记录");
        }

        // 合并完数据还得读得出来
        let read = store.read_only().unwrap();
        assert_eq!(read.total().unwrap(), 30);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 老库升级：`pinned` 是后加的列，`CREATE TABLE IF NOT EXISTS` 不会补。
    /// 用户手上已经有 48 条数据，这条迁移路径必须验证，不能只测新库。
    #[test]
    fn migrates_old_db_without_pinned_column() {
        let dir = std::env::temp_dir().join(format!("rpaste-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 造一个跟升级前一模一样的库（没有 pinned 列）
        {
            let conn = Connection::open(dir.join("rpaste.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE clips (
                     id INTEGER PRIMARY KEY, kind INTEGER NOT NULL, hash BLOB NOT NULL,
                     preview TEXT NOT NULL, byte_len INTEGER NOT NULL, inline BLOB,
                     blob_path TEXT, created_at INTEGER NOT NULL,
                     hit_count INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO clips (kind, hash, preview, byte_len, inline, created_at)
                 VALUES (0, x'00', '老数据', 9, x'6F6C64', 1000);",
            )
            .unwrap();
        }

        let mut store = Store::open(&dir).unwrap();

        let read = store.read_only().unwrap();
        let list = read.recent(10).unwrap();
        assert_eq!(list.len(), 1, "升级不能丢数据");
        assert_eq!(list[0].preview, "老数据");
        assert!(!list[0].pinned, "补的列默认值应是未收藏");

        // 补出来的列要能正常读写
        store.set_pinned(list[0].id, true).unwrap();
        let read = store.read_only().unwrap();
        assert!(read.recent(1).unwrap()[0].pinned);
        // 老数据要能被索引读到，否则升级后「全库只剩新条目能搜」。
        assert_eq!(read.all_meta().unwrap().len(), 1);

        // 再 open 一次，确认不会重复 ALTER 报 duplicate column
        drop(store);
        assert!(Store::open(&dir).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 老版本的 FTS 索引表必须在升级时被清掉：检索已经改成内存索引，
    /// 留着它白占 1/3 库体积，触发器还让每次写入多一次索引维护。
    #[test]
    fn drops_legacy_fts_table_on_open() {
        let dir = std::env::temp_dir().join(format!("rpaste-drop-fts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let conn = Connection::open(dir.join("rpaste.db")).unwrap();
            conn.execute_batch("CREATE TABLE clips_fts (preview TEXT);")
                .unwrap();
        }

        let store = Store::open(&dir).unwrap();
        let left: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name LIKE 'clips_fts%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0, "旧的 FTS 表应被删除");

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// 尾部正文要能原样落库、原样读回，且不进 `preview`（预览仍然只有 512 字）。
    #[test]
    fn tail_round_trips_separately_from_preview() {
        let (mut store, dir) = temp_store();
        let head = "开".repeat(PREVIEW_CHARS);
        let text = format!("{head}后半段的关键词");
        let (preview, tail) = crate::model::make_preview_and_tail(&text, PREVIEW_CHARS, TAIL_CHARS);
        assert!(!tail.is_empty());

        let c = Captured::new(Kind::Text, text.clone().into_bytes(), preview.clone())
            .with_tail(tail.clone());
        let InsertOutcome::New(id) = store.insert(&c).unwrap() else {
            panic!("应该是新条目");
        };

        let read = store.read_only().unwrap();
        // 预览保持 512 字，列表不会被一条长文撑爆。
        assert_eq!(
            read.recent(1).unwrap()[0].preview.chars().count(),
            PREVIEW_CHARS
        );
        assert_eq!(read.all_tails().unwrap(), vec![(id, tail.clone())]);
        assert_eq!(read.tails_since(0).unwrap(), vec![(id, tail)]);
        // 短条目不该出现在尾部查询里——那会让每次刷新白读一堆空串。
        assert!(read.tails_since(id).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 老库升级：之前只有 512 字预览，长文本后半段搜不到。升级时要真的把
    /// 尾部补出来，否则用户手里的老条目「修了还是搜不到」。
    #[test]
    fn migrates_old_db_and_backfills_tails() {
        let dir = std::env::temp_dir().join(format!("rpaste-tail-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let long_text = format!("{}{}", "开".repeat(PREVIEW_CHARS), "后半段的关键词");
        {
            let conn = Connection::open(dir.join("rpaste.db")).unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE clips (
                     id INTEGER PRIMARY KEY, kind INTEGER NOT NULL, hash BLOB NOT NULL,
                     preview TEXT NOT NULL, byte_len INTEGER NOT NULL, inline BLOB,
                     blob_path TEXT, created_at INTEGER NOT NULL,
                     hit_count INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO clips (kind, hash, preview, byte_len, inline, created_at)
                 VALUES (0, x'00', '{}', {}, x'{}', 1000);",
                "开".repeat(PREVIEW_CHARS),
                long_text.len(),
                long_text
                    .as_bytes()
                    .iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<String>()
            ))
            .unwrap();
        }

        let store = Store::open(&dir).unwrap();
        let read = store.read_only().unwrap();
        let tails = read.all_tails().unwrap();
        assert_eq!(tails.len(), 1, "老数据要补出尾部");
        assert!(tails[0].1.contains("后半段的关键词"), "尾部内容要对得上");

        drop(store);
        // 再 open 一次：`user_version` 记住做过，不该重复回填（也不该报错）。
        assert!(Store::open(&dir).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
