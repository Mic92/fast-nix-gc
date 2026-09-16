//! SQLite store database access and in-memory reference graph.

use crate::HashMap;
use anyhow::{Context, Result, anyhow};
use harmonia_store_core::store_path::{StoreDir, StorePath};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

/// Handle to the Nix store SQLite database.
pub struct NixDb {
    pub conn: Connection,
    pub store_dir: PathBuf,
    pub state_dir: PathBuf,
    pub real_store_dir: PathBuf,
    pub links_dir: PathBuf,
    /// Mirror of Nix's `keep-derivations` setting (default: true).
    /// When set, an alive path keeps its deriver (`ValidPaths.deriver`)
    /// alive.
    pub keep_derivations: bool,
    /// Mirror of Nix's `keep-outputs` setting (default: false).
    /// When set, an alive .drv file keeps its outputs alive.
    pub keep_outputs: bool,
}

impl NixDb {
    pub fn open(store_dir: &Path, state_dir: &Path) -> Result<Self> {
        Self::open_with_mode(store_dir, state_dir, false)
    }

    /// Read-only open for dry runs: no journal-mode flip, no write lock,
    /// works on a read-only filesystem.
    pub fn open_read_only(store_dir: &Path, state_dir: &Path) -> Result<Self> {
        Self::open_with_mode(store_dir, state_dir, true)
    }

    fn open_with_mode(store_dir: &Path, state_dir: &Path, read_only: bool) -> Result<Self> {
        // "/nix/store/" must equal "/nix/store": every prefix comparison
        // against DB paths appends its own '/'. A trailing slash would make
        // the basename index empty and the whole store look dead.
        let store_dir = normalize_dir(store_dir);
        let store_dir = store_dir.as_path();
        let db_path = state_dir.join("db/db.sqlite");
        let rw_flag = if read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let conn = Connection::open_with_flags(&db_path, rw_flag | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_context(|| format!("opening {}", db_path.display()))?;

        if !read_only {
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
        }
        // Nix's schema relies on FK actions (DerivationOutputs/Refs rows
        // cascade when a ValidPaths row goes away), but SQLite only honors
        // them with the pragma on. Without it every invalidation leaks
        // orphaned DerivationOutputs rows.
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // nix-daemon may hold short write locks; fail with SQLITE_BUSY only
        // after a grace period, like Nix's busy handler.
        conn.busy_timeout(std::time::Duration::from_secs(60))?;

        // Like Nix's realStoreDir: the physical directory when store_dir
        // is a symlink (the DB keeps logical paths, disk ops want real
        // ones). Falls back to the logical dir if it can't be resolved.
        let real_store_dir =
            std::fs::canonicalize(store_dir).unwrap_or_else(|_| store_dir.to_path_buf());
        Ok(NixDb {
            conn,
            store_dir: store_dir.to_path_buf(),
            state_dir: state_dir.to_path_buf(),
            links_dir: real_store_dir.join(".links"),
            real_store_dir,
            // Read the resolved nix.conf via `nix config show`; defaults
            // match Nix if it's not in PATH.
            keep_derivations: crate::nix_config::bool_setting("keep-derivations", true),
            keep_outputs: crate::nix_config::bool_setting("keep-outputs", false),
        })
    }

    /// Check whether a table exists in the SQLite database.
    fn has_table(conn: &Connection, name: &str) -> Result<bool> {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
            [name],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// Load the full reference graph into memory. Walking it as an
    /// in-memory CSR is far cheaper than N point queries.
    pub fn load_graph(&self) -> Result<StoreGraph> {
        // Both queries must see the same snapshot, otherwise a path
        // registered between them ends up with missing edges.
        self.conn.execute_batch("BEGIN")?;
        let result = self.load_graph_in_txn();
        if result.is_err() {
            // Leave the connection usable for the caller.
            self.conn.execute_batch("ROLLBACK").ok();
        } else {
            self.conn.execute_batch("COMMIT")?;
        }
        result
    }

    fn load_graph_in_txn(&self) -> Result<StoreGraph> {
        // ids are dense autoincrement, so a Vec works as the id->idx map.
        let max_id: i64 =
            self.conn
                .query_row("SELECT IFNULL(MAX(id), 0) FROM ValidPaths", [], |r| {
                    r.get(0)
                })?;
        const MISSING: u32 = u32::MAX;
        let mut id_to_idx = vec![MISSING; (max_id as usize) + 1];

        let mut paths: Vec<String> = Vec::new();
        let mut ids: Vec<i64> = Vec::new();
        let mut nar_sizes: Vec<u64> = Vec::new();
        let mut registration_times: Vec<i64> = Vec::new();
        let t_nodes = std::time::Instant::now();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT id, path, narSize, registrationTime FROM ValidPaths")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                let path: String = row.get(1)?;
                let nar: Option<i64> = row.get(2)?;
                let reg_time: i64 = row.get(3)?;
                let idx = paths.len() as u32;
                if (id as usize) < id_to_idx.len() {
                    id_to_idx[id as usize] = idx;
                }
                paths.push(path);
                ids.push(id);
                nar_sizes.push(nar.unwrap_or(0).max(0) as u64);
                registration_times.push(reg_time);
            }
        }

        log::debug!(
            "loaded {} nodes in {:.1}s",
            paths.len(),
            t_nodes.elapsed().as_secs_f64()
        );

        // CSR adjacency: flat target array + per-node offsets.
        let n = paths.len();
        let mut edges: Vec<(u32, u32)> = Vec::new();
        let t_edges = std::time::Instant::now();

        let mut add_edges =
            |conn: &Connection, sql: &str, params: &[&dyn rusqlite::ToSql]| -> Result<()> {
                let start = std::time::Instant::now();
                let before = edges.len();
                let mut stmt = conn.prepare(sql)?;
                let mut rows = stmt.query(params)?;
                while let Some(row) = rows.next()? {
                    let from_id: i64 = row.get(0)?;
                    let to_id: i64 = row.get(1)?;
                    let from = *id_to_idx.get(from_id as usize).unwrap_or(&MISSING);
                    let to = *id_to_idx.get(to_id as usize).unwrap_or(&MISSING);
                    if from != MISSING && to != MISSING {
                        edges.push((from, to));
                    }
                }
                log::debug!(
                    "edge query took {:.1}s ({} edges): {sql}",
                    start.elapsed().as_secs_f64(),
                    edges.len() - before,
                );
                Ok(())
            };

        // Refs table: direct references between store paths.
        add_edges(&self.conn, "SELECT referrer, reference FROM Refs", &[])?;

        // Edge directions mirror exactly what Nix's GC propagates
        // (gc.cc maybeDeleteReferrersClosure + misc.cc computeFSClosure
        // with includeOutputs = keep-outputs, includeDerivers =
        // keep-derivations):
        //
        //   keep-derivations: an alive path keeps `info->deriver` alive.
        //     Edge output→drv via ValidPaths.deriver ONLY. The referrer
        //     walk's condition (output of drv with deriver == drv) is a
        //     subset of this. Nix does NOT pin a drv merely because a
        //     DerivationOutputs/BuildTraceV3 row maps one of its outputs:
        //     if the output's deriver is a different (newer) drv, the old
        //     drv is garbage.
        //
        //   keep-outputs: an alive drv keeps its outputs alive.
        //     Edge drv→output via the drv's output map: DerivationOutputs
        //     for input-addressed outputs, BuildTraceV3 for CA/dynamic
        //     outputs (Nix ≥2.35; drvPath is a store path *basename*,
        //     outputPath a full store path; created lazily, skip if
        //     absent), plus ValidPaths.deriver. The deriver edge is a
        //     subset of Nix's relation (deriver == drv implies the path is
        //     in the drv's output map) and is the only usable drv→output
        //     mapping for CA outputs of Nix ≤2.34, whose Realisations
        //     table keys drvPath by content hash and so can't be joined
        //     against ValidPaths.
        let has_build_trace = self.keep_outputs && Self::has_table(&self.conn, "BuildTraceV3")?;
        let store_prefix = format!("{}/", self.store_dir.display());

        if self.keep_derivations {
            // output → drv via ValidPaths.deriver
            add_edges(
                &self.conn,
                "SELECT v.id, d.id FROM ValidPaths v \
                 JOIN ValidPaths d ON d.path = v.deriver \
                 WHERE v.deriver IS NOT NULL",
                &[],
            )?;
        }

        if self.keep_outputs {
            // drv → output via DerivationOutputs
            add_edges(
                &self.conn,
                "SELECT do2.drv, o.id FROM DerivationOutputs do2 \
                 JOIN ValidPaths o ON o.path = do2.path",
                &[],
            )?;
            // drv → output via ValidPaths.deriver (covers Realisations-era
            // CA outputs; see the edge-direction comment above)
            add_edges(
                &self.conn,
                "SELECT d.id, v.id FROM ValidPaths v \
                 JOIN ValidPaths d ON d.path = v.deriver \
                 WHERE v.deriver IS NOT NULL",
                &[],
            )?;
            if has_build_trace {
                // drv → output via BuildTraceV3
                add_edges(
                    &self.conn,
                    "SELECT d.id, o.id FROM BuildTraceV3 bt \
                     JOIN ValidPaths d ON d.path = ? || bt.drvPath \
                     JOIN ValidPaths o ON o.path = bt.outputPath",
                    &[&store_prefix],
                )?;
            }
        }

        log::debug!(
            "loaded {} edges in {:.1}s",
            edges.len(),
            t_edges.elapsed().as_secs_f64()
        );

        // CSR offsets are u32; refuse to build a graph the index type
        // cannot address instead of silently wrapping.
        anyhow::ensure!(
            edges.len() <= u32::MAX as usize,
            "store has more than 2^32 reference edges"
        );

        let mut ref_offsets = vec![0u32; n + 1];
        for &(from, _) in &edges {
            ref_offsets[from as usize + 1] += 1;
        }
        for i in 0..n {
            ref_offsets[i + 1] += ref_offsets[i];
        }
        let mut ref_targets = vec![0u32; edges.len()];
        let mut cursor = ref_offsets.clone();
        for &(from, to) in &edges {
            let pos = cursor[from as usize];
            ref_targets[pos as usize] = to;
            cursor[from as usize] += 1;
        }

        Ok(StoreGraph {
            paths,
            ids,
            nar_sizes,
            registration_times,
            ref_offsets,
            ref_targets,
            store_prefix: format!("{}/", self.store_dir.display()),
        })
    }

    /// `StoreDir` view of `store_dir`. Fails if it isn't valid UTF-8.
    pub fn store_dir_typed(&self) -> Result<StoreDir> {
        StoreDir::new(self.store_dir.clone())
            .map_err(|e| anyhow!("{}: {e}", self.store_dir.display()))
    }

    /// Return all valid store paths parsed into `StorePath`. Rows that
    /// don't parse (corrupt DB) are returned as an error rather than
    /// silently dropped.
    pub fn valid_store_paths(&self) -> Result<Vec<StorePath>> {
        let store_dir = self.store_dir_typed()?;
        let mut stmt = self.conn.prepare("SELECT path FROM ValidPaths")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|r| {
            let s = r?;
            store_dir
                .parse::<StorePath>(&s)
                .map_err(|e| anyhow!("invalid store path '{s}': {e}"))
        })
        .collect()
    }

    pub fn is_valid_path(&self, path: &str) -> Result<bool> {
        // Cached: called once per unknown-on-disk entry, which can be
        // many thousands after an interrupted nixos-install or nix copy.
        let mut stmt = self
            .conn
            .prepare_cached("SELECT COUNT(*) FROM ValidPaths WHERE path = ?")?;
        let n: i64 = stmt.query_row([path], |r| r.get(0))?;
        Ok(n > 0)
    }

    /// Remove paths by their ValidPaths.id in a single transaction. The
    /// caller holds the ids from `load_graph`, so deletion is a rowid
    /// insert into DeadPaths rather than a per-path index lookup.
    pub fn invalidate_ids(&self, ids: impl Iterator<Item = i64>) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<()> {
            // Collect the dead ids in a temp table so their Refs can be
            // batch-deleted in two statements instead of one subquery
            // per path.
            self.conn.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS DeadPaths (id INTEGER PRIMARY KEY)",
            )?;
            self.conn.execute_batch("DELETE FROM DeadPaths")?;
            {
                let mut ins = self.conn.prepare("INSERT INTO DeadPaths VALUES (?)")?;
                for id in ids {
                    ins.execute([id])?;
                }
            }
            // Delete reference edges involving dead paths first.
            // Cycles among dead paths (A→B, B→A) would otherwise
            // violate the FK `ON DELETE RESTRICT` on Refs.reference.
            self.conn.execute_batch(
                "DELETE FROM Refs WHERE referrer IN (SELECT id FROM DeadPaths) \
                 OR reference IN (SELECT id FROM DeadPaths)",
            )?;
            // Same for the CA realisations of Nix ≤2.34:
            // RealisationsRefs.realisationReference is ON DELETE RESTRICT,
            // and our bulk DELETE is unordered (Nix avoids the constraint
            // by deleting one path at a time in topological order).
            if Self::has_table(&self.conn, "Realisations")? {
                self.conn.execute_batch(
                    "DELETE FROM RealisationsRefs WHERE referrer IN \
                         (SELECT id FROM Realisations WHERE outputPath IN \
                             (SELECT id FROM DeadPaths)) \
                     OR realisationReference IN \
                         (SELECT id FROM Realisations WHERE outputPath IN \
                             (SELECT id FROM DeadPaths)); \
                     DELETE FROM Realisations WHERE outputPath IN \
                         (SELECT id FROM DeadPaths)",
                )?;
            }
            // Nix >= 2.35: outputPath is text, nothing cascades.
            if Self::has_table(&self.conn, "BuildTraceV3")? {
                self.conn.execute_batch(
                    "DELETE FROM BuildTraceV3 WHERE outputPath IN \
                         (SELECT path FROM ValidPaths WHERE id IN (SELECT id FROM DeadPaths))",
                )?;
            }
            self.conn
                .execute_batch("DELETE FROM ValidPaths WHERE id IN (SELECT id FROM DeadPaths)")?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                // Truncate the WAL back to the main db so its disk use
                // doesn't accumulate across chunks. On a full disk an
                // unbounded WAL would abort a later chunk. Best effort:
                // a blocked checkpoint just leaves the WAL for the next.
                let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
                Ok(())
            }
            Err(e) => {
                self.conn.execute_batch("ROLLBACK").ok();
                Err(e)
            }
        }
    }

    /// VACUUM if enough of the file is free pages to be worth a full
    /// rewrite: at least a quarter of the file and at least 64 pages
    /// (256 KiB at the default page size). The caller must hold the
    /// exclusive gc.lock so no Nix writer collides; concurrent readers
    /// are fine in WAL mode. Returns whether a vacuum ran.
    pub fn maybe_vacuum(&self) -> Result<bool> {
        let freelist: i64 = self
            .conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        let pages: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        if freelist < 64 || freelist * 4 < pages {
            return Ok(false);
        }
        log::info!("vacuuming database ({freelist} of {pages} pages free)...");
        let start = std::time::Instant::now();
        self.conn.execute_batch("VACUUM")?;
        // Best effort: a concurrent reader may block the WAL truncate;
        // the next checkpoint picks it up.
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
        log::info!("vacuum done in {:.1}s", start.elapsed().as_secs_f64());
        Ok(true)
    }
}

/// Strip trailing slashes (but keep a bare "/").
fn normalize_dir(dir: &Path) -> PathBuf {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let bytes = dir.as_os_str().as_bytes();
    let mut end = bytes.len();
    while end > 1 && bytes[end - 1] == b'/' {
        end -= 1;
    }
    PathBuf::from(std::ffi::OsString::from_vec(bytes[..end].to_vec()))
}

/// In-memory snapshot of the store reference graph in CSR layout.
pub struct StoreGraph {
    /// node idx -> store path string
    pub paths: Vec<String>,
    /// node idx -> ValidPaths.id (DB primary key). Lets invalidation
    /// delete by rowid instead of re-resolving each path string.
    pub ids: Vec<i64>,
    /// node idx -> narSize (bytes)
    pub nar_sizes: Vec<u64>,
    /// node idx -> registrationTime (Unix epoch seconds)
    pub registration_times: Vec<i64>,
    /// CSR row offsets: refs of node i are ref_targets[ref_offsets[i]..ref_offsets[i+1]]
    pub ref_offsets: Vec<u32>,
    /// CSR column indices: flat array of referenced node idxs
    pub ref_targets: Vec<u32>,
    /// store dir prefix including trailing slash, e.g. "/nix/store/"
    pub store_prefix: String,
}

/// Basename -> idx index for the lookups during root finding and the
/// unknown-on-disk scan. Borrows from the StoreGraph; built once.
pub struct BasenameIndex<'g> {
    pub map: HashMap<&'g str, u32>,
    pub store_prefix: &'g str,
}

impl<'g> BasenameIndex<'g> {
    pub fn new(graph: &'g StoreGraph) -> Self {
        let mut map: HashMap<&'g str, u32> = HashMap::default();
        map.reserve(graph.paths.len());
        for (i, p) in graph.paths.iter().enumerate() {
            if let Some(b) = p.strip_prefix(&graph.store_prefix) {
                map.insert(b, i as u32);
            }
        }
        BasenameIndex {
            map,
            store_prefix: &graph.store_prefix,
        }
    }

    pub fn idx_of(&self, path: &str) -> Option<u32> {
        let b = path.strip_prefix(self.store_prefix)?;
        self.map.get(b).copied()
    }

    pub fn idx_of_basename(&self, basename: &str) -> Option<u32> {
        self.map.get(basename).copied()
    }
}

impl StoreGraph {
    /// Graph with no nodes, used while the real graph is still loading.
    pub fn empty(store_prefix: String) -> StoreGraph {
        StoreGraph {
            paths: Vec::new(),
            ids: Vec::new(),
            nar_sizes: Vec::new(),
            registration_times: Vec::new(),
            ref_offsets: vec![0],
            ref_targets: Vec::new(),
            store_prefix,
        }
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    #[inline]
    pub fn refs(&self, node: u32) -> &[u32] {
        let start = self.ref_offsets[node as usize] as usize;
        let end = self.ref_offsets[node as usize + 1] as usize;
        &self.ref_targets[start..end]
    }

    /// Mark all nodes reachable from the given root indices.
    pub fn compute_closure(&self, roots: &[u32]) -> Vec<bool> {
        let mut alive = vec![false; self.len()];
        let mut stack: Vec<u32> = Vec::with_capacity(roots.len());
        for &r in roots {
            if !alive[r as usize] {
                alive[r as usize] = true;
                stack.push(r);
            }
        }
        while let Some(node) = stack.pop() {
            for &next in self.refs(node) {
                if !alive[next as usize] {
                    alive[next as usize] = true;
                    stack.push(next);
                }
            }
        }
        alive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const H2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const H3: &str = "cccccccccccccccccccccccccccccccc";

    struct TestDb {
        #[allow(dead_code)] // keep tempdir alive
        dir: tempfile::TempDir,
        db: NixDb,
    }

    fn full(name: &str) -> String {
        format!("/nix/store/{name}")
    }

    fn setup() -> TestDb {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(state.join("db")).unwrap();
        let conn = Connection::open(state.join("db/db.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE ValidPaths (
                 id integer primary key autoincrement not null,
                 path text unique not null,
                 hash text not null,
                 registrationTime integer not null,
                 deriver text,
                 narSize integer
             );
             CREATE TABLE Refs (
                 referrer integer not null,
                 reference integer not null,
                 primary key (referrer, reference),
                 foreign key (reference) references ValidPaths(id) on delete restrict
             );
             CREATE TABLE DerivationOutputs (
                 drv integer not null,
                 id text not null,
                 path text not null,
                 primary key (drv, id),
                 foreign key (drv) references ValidPaths(id) on delete cascade
             );",
        )
        .unwrap();
        drop(conn);
        let mut db = NixDb::open(Path::new("/nix/store"), &state).unwrap();
        // Pin settings; NixDb::open reads them from the host's nix.conf.
        db.keep_derivations = false;
        db.keep_outputs = false;
        TestDb { dir, db }
    }

    fn add_path(db: &NixDb, name: &str, nar_size: i64, reg_time: i64) -> i64 {
        db.conn
            .execute(
                "INSERT INTO ValidPaths (path, hash, registrationTime, narSize) \
                 VALUES (?, 'sha256:x', ?, ?)",
                rusqlite::params![full(name), reg_time, nar_size],
            )
            .unwrap();
        db.conn.last_insert_rowid()
    }

    fn add_ref(db: &NixDb, referrer: i64, reference: i64) {
        db.conn
            .execute(
                "INSERT INTO Refs (referrer, reference) VALUES (?, ?)",
                rusqlite::params![referrer, reference],
            )
            .unwrap();
    }

    #[test]
    fn maybe_vacuum_skips_small_freelist() {
        let t = setup();
        add_path(&t.db, &format!("{H1}-a"), 1, 1);
        assert!(!t.db.maybe_vacuum().unwrap());
    }

    #[test]
    fn has_table_checks_existence() {
        let t = setup();
        assert!(NixDb::has_table(&t.db.conn, "ValidPaths").unwrap());
        assert!(!NixDb::has_table(&t.db.conn, "NoSuchTable").unwrap());
    }

    #[test]
    fn load_graph_builds_csr() {
        let t = setup();
        let a = add_path(&t.db, &format!("{H1}-a"), 100, 1000);
        let b = add_path(&t.db, &format!("{H2}-b"), 200, 2000);
        let c = add_path(&t.db, &format!("{H3}-c"), 300, 3000);
        // Two edges from one node exercise the CSR cursor advance.
        add_ref(&t.db, a, b);
        add_ref(&t.db, a, c);

        let g = t.db.load_graph().unwrap();
        assert_eq!(g.len(), 3);
        assert!(!g.is_empty());
        let ia = g
            .paths
            .iter()
            .position(|p| p == &full(&format!("{H1}-a")))
            .unwrap();
        let ib = g
            .paths
            .iter()
            .position(|p| p == &full(&format!("{H2}-b")))
            .unwrap();
        let ic = g
            .paths
            .iter()
            .position(|p| p == &full(&format!("{H3}-c")))
            .unwrap();
        assert_eq!(g.nar_sizes[ia], 100);
        assert_eq!(g.registration_times[ic], 3000);
        let mut refs_a = g.refs(ia as u32).to_vec();
        refs_a.sort();
        let mut expected = vec![ib as u32, ic as u32];
        expected.sort();
        assert_eq!(refs_a, expected);
        assert!(g.refs(ib as u32).is_empty());
        assert!(g.refs(ic as u32).is_empty());
    }

    #[test]
    fn empty_graph() {
        let t = setup();
        let g = t.db.load_graph().unwrap();
        assert_eq!(g.len(), 0);
        assert!(g.is_empty());
    }

    #[test]
    fn store_dir_typed_uses_configured_dir() {
        let t = setup();
        // Re-open with a non-default store dir; the typed view must not
        // fall back to /nix/store.
        let db = NixDb::open(Path::new("/custom/store"), &t.db.state_dir).unwrap();
        assert_eq!(db.store_dir_typed().unwrap().to_string(), "/custom/store");
    }

    #[test]
    fn load_graph_ignores_edges_with_unknown_ids() {
        let t = setup();
        let a = add_path(&t.db, &format!("{H1}-a"), 100, 1000);
        // Dangling edges must be dropped, not crash or corrupt the CSR.
        // FK enforcement default varies by SQLite build; this test wants
        // the corrupt rows.
        t.db.conn
            .pragma_update(None, "foreign_keys", "OFF")
            .unwrap();
        t.db.conn
            .execute_batch(&format!("INSERT INTO Refs VALUES ({a}, 9999), (9999, {a})"))
            .unwrap();
        let g = t.db.load_graph().unwrap();
        assert!(g.refs(0).is_empty());
    }

    #[test]
    fn load_graph_keep_derivations_adds_output_to_drv_edge() {
        let t = setup();
        let mut db = t.db;
        let drv = add_path(&db, &format!("{H1}-pkg.drv"), 10, 1000);
        let out = add_path(&db, &format!("{H2}-pkg"), 100, 1000);
        db.conn
            .execute(
                "UPDATE ValidPaths SET deriver = ? WHERE id = ?",
                rusqlite::params![full(&format!("{H1}-pkg.drv")), out],
            )
            .unwrap();
        let _ = drv;

        db.keep_derivations = true;
        let g = db.load_graph().unwrap();
        let iout = g.paths.iter().position(|p| p.ends_with("-pkg")).unwrap() as u32;
        let idrv = g.paths.iter().position(|p| p.ends_with(".drv")).unwrap() as u32;
        assert_eq!(g.refs(iout), &[idrv]);
        assert!(g.refs(idrv).is_empty());

        // keep-outputs: the deriver field alone pins the output (it is
        // the only drv→output mapping for Realisations-era CA outputs).
        db.keep_outputs = true;
        let g = db.load_graph().unwrap();
        assert_eq!(g.refs(idrv), &[iout]);
    }

    #[test]
    fn load_graph_build_trace_edges() {
        let t = setup();
        let mut db = t.db;
        let drv_base = format!("{H1}-pkg.drv");
        add_path(&db, &drv_base, 10, 1000);
        add_path(&db, &format!("{H2}-pkg"), 100, 1000);
        db.conn
            .execute_batch(&format!(
                "CREATE TABLE BuildTraceV3 (
                     id integer primary key autoincrement not null,
                     drvPath text not null,
                     outputName text not null,
                     outputPath text not null,
                     signatures text
                 );
                 INSERT INTO BuildTraceV3 (drvPath, outputName, outputPath)
                 VALUES ('{drv_base}', 'out', '{}');",
                full(&format!("{H2}-pkg"))
            ))
            .unwrap();

        // keep-derivations pins drvs only via the deriver field; a
        // BuildTraceV3 row alone must not create an output→drv edge
        // (the output may have been rebuilt by a newer drv).
        db.keep_derivations = true;
        let g = db.load_graph().unwrap();
        let iout = g.paths.iter().position(|p| p.ends_with("-pkg")).unwrap() as u32;
        let idrv = g.paths.iter().position(|p| p.ends_with(".drv")).unwrap() as u32;
        assert!(g.refs(iout).is_empty());

        // keep-outputs pins CA outputs of an alive drv via BuildTraceV3.
        db.keep_derivations = false;
        db.keep_outputs = true;
        let g = db.load_graph().unwrap();
        assert_eq!(g.refs(idrv), &[iout]);
    }

    #[test]
    fn valid_store_paths_and_typed_variants() {
        let t = setup();
        add_path(&t.db, &format!("{H1}-a"), 1, 1);
        add_path(&t.db, &format!("{H2}-b"), 1, 1);

        let mut paths: Vec<String> =
            t.db.valid_store_paths()
                .unwrap()
                .iter()
                .map(|p| format!("/nix/store/{p}"))
                .collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![full(&format!("{H1}-a")), full(&format!("{H2}-b"))]
        );

        let sd = t.db.store_dir_typed().unwrap();
        assert_eq!(sd.to_string(), "/nix/store");

        let sps = t.db.valid_store_paths().unwrap();
        assert_eq!(sps.len(), 2);
        let names: Vec<_> = sps.iter().map(|p| p.to_string()).collect();
        assert!(names.iter().any(|n| n.ends_with("-a")), "{names:?}");
    }

    #[test]
    fn is_valid_path_checks_db() {
        let t = setup();
        add_path(&t.db, &format!("{H1}-a"), 1, 1);
        assert!(t.db.is_valid_path(&full(&format!("{H1}-a"))).unwrap());
        assert!(!t.db.is_valid_path(&full(&format!("{H2}-b"))).unwrap());
    }

    #[test]
    fn invalidate_ids_handles_ref_cycles() {
        let t = setup();
        let a = add_path(&t.db, &format!("{H1}-a"), 1, 1);
        let b = add_path(&t.db, &format!("{H2}-b"), 1, 1);
        add_path(&t.db, &format!("{H3}-c"), 1, 1);
        // Cycle between the two dead paths; FK on delete restrict would
        // reject row-by-row deletion.
        add_ref(&t.db, a, b);
        add_ref(&t.db, b, a);

        t.db.invalidate_ids([a, b].into_iter()).unwrap();

        let remaining: Vec<String> =
            t.db.valid_store_paths()
                .unwrap()
                .iter()
                .map(|p| format!("/nix/store/{p}"))
                .collect();
        assert_eq!(remaining, vec![full(&format!("{H3}-c"))]);
        let refs: i64 =
            t.db.conn
                .query_row("SELECT COUNT(*) FROM Refs", [], |r| r.get(0))
                .unwrap();
        assert_eq!(refs, 0);
    }

    #[test]
    fn invalidate_ids_cascades_derivation_outputs() {
        let t = setup();
        let drv = add_path(&t.db, &format!("{H1}-pkg.drv"), 1, 1);
        let out = add_path(&t.db, &format!("{H2}-pkg"), 1, 1);
        t.db.conn
            .execute(
                "INSERT INTO DerivationOutputs (drv, id, path) VALUES (?, 'out', ?)",
                rusqlite::params![drv, full(&format!("{H2}-pkg"))],
            )
            .unwrap();
        let _ = out;

        t.db.invalidate_ids([drv].into_iter()).unwrap();

        let n: i64 =
            t.db.conn
                .query_row("SELECT COUNT(*) FROM DerivationOutputs", [], |r| r.get(0))
                .unwrap();
        assert_eq!(n, 0, "DerivationOutputs row orphaned");
    }

    #[test]
    fn invalidate_ids_clears_realisations_with_restrict_fk() {
        // Nix ≤2.34 CA schema: RealisationsRefs.realisationReference is ON
        // DELETE RESTRICT. Bulk-deleting two dead paths whose realisations
        // reference each other must not trip the constraint.
        let t = setup();
        t.db.conn
            .execute_batch(
                "CREATE TABLE Realisations (
                     id integer primary key autoincrement not null,
                     drvPath text not null,
                     outputName text not null,
                     outputPath integer not null references ValidPaths(id) on delete cascade,
                     signatures text
                 );
                 CREATE TABLE RealisationsRefs (
                     referrer integer not null,
                     realisationReference integer,
                     foreign key (referrer) references Realisations(id) on delete cascade,
                     foreign key (realisationReference) references Realisations(id) on delete restrict
                 );",
            )
            .unwrap();
        let a = add_path(&t.db, &format!("{H1}-a"), 1, 1);
        let b = add_path(&t.db, &format!("{H2}-b"), 1, 1);
        t.db.conn
            .execute_batch(&format!(
                "INSERT INTO Realisations (drvPath, outputName, outputPath) \
                     VALUES ('sha256:a!out', 'out', {a}), ('sha256:b!out', 'out', {b});
                 INSERT INTO RealisationsRefs (referrer, realisationReference) VALUES (1, 2);"
            ))
            .unwrap();

        t.db.invalidate_ids([a, b].into_iter()).unwrap();

        for table in ["Realisations", "RealisationsRefs", "ValidPaths"] {
            let n: i64 =
                t.db.conn
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                    .unwrap();
            assert_eq!(n, 0, "{table} not cleared");
        }
    }

    #[test]
    fn invalidate_ids_clears_build_trace() {
        // A stale row makes Nix refuse to register a differing rebuild.
        let t = setup();
        t.db.conn
            .execute_batch(
                "CREATE TABLE BuildTraceV3 (
                     id integer primary key autoincrement not null,
                     drvPath text not null,
                     outputName text not null,
                     outputPath text not null,
                     signatures text
                 );",
            )
            .unwrap();
        let a = add_path(&t.db, &format!("{H1}-a"), 1, 1);
        add_path(&t.db, &format!("{H2}-b"), 1, 1);
        t.db.conn
            .execute_batch(&format!(
                "INSERT INTO BuildTraceV3 (drvPath, outputName, outputPath) \
                     VALUES ('{H3}-a.drv', 'out', '{}'), ('{H3}-b.drv', 'out', '{}');",
                full(&format!("{H1}-a")),
                full(&format!("{H2}-b"))
            ))
            .unwrap();

        t.db.invalidate_ids([a].into_iter()).unwrap();

        let left: Vec<String> =
            t.db.conn
                .prepare("SELECT outputPath FROM BuildTraceV3")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect();
        assert_eq!(left, [full(&format!("{H2}-b"))]);
    }

    #[test]
    fn basename_index_lookups() {
        let t = setup();
        add_path(&t.db, &format!("{H1}-a"), 1, 1);
        add_path(&t.db, &format!("{H2}-b"), 1, 1);
        add_path(&t.db, &format!("{H3}-c"), 1, 1);
        let g = t.db.load_graph().unwrap();
        let bidx = BasenameIndex::new(&g);

        let ic = g.paths.iter().position(|p| p.ends_with("-c")).unwrap() as u32;
        assert_eq!(bidx.idx_of(&full(&format!("{H3}-c"))), Some(ic));
        assert_eq!(bidx.idx_of_basename(&format!("{H3}-c")), Some(ic));
        assert_eq!(bidx.idx_of("/elsewhere/x"), None);
        assert_eq!(bidx.idx_of_basename("nope"), None);
    }

    #[test]
    fn compute_closure_marks_reachable_only() {
        let t = setup();
        let a = add_path(&t.db, &format!("{H1}-a"), 1, 1);
        let b = add_path(&t.db, &format!("{H2}-b"), 1, 1);
        let c = add_path(&t.db, &format!("{H3}-c"), 1, 1);
        add_ref(&t.db, a, b);
        // Cycle must terminate.
        add_ref(&t.db, b, a);
        let _ = c;

        let g = t.db.load_graph().unwrap();
        let ia = g.paths.iter().position(|p| p.ends_with("-a")).unwrap() as u32;
        let ic = g.paths.iter().position(|p| p.ends_with("-c")).unwrap();
        // Duplicate roots must not double-visit.
        let alive = g.compute_closure(&[ia, ia]);
        assert_eq!(alive.len(), 3);
        assert_eq!(alive.iter().filter(|&&x| x).count(), 2);
        assert!(!alive[ic]);
    }
}
