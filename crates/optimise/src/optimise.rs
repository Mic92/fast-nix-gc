//! Hardlink-based store dedup, on-disk compatible with `nix-store --optimise`.

use crate::hash::nar_hash_nix32;
use anyhow::{Context, Result, bail};
use fast_nix_common::{
    HashSet, db::NixDb, format_size, make_store_writable, unshare_mount_namespace,
};
use harmonia_store_core::store_path::StoreDir;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use walkdir::WalkDir;

#[derive(Default, Debug)]
pub struct Stats {
    pub files_linked: AtomicU64,
    pub bytes_freed: AtomicU64,
    pub files_skipped: AtomicU64,
    pub link_enospc: AtomicU64,
    /// Files or paths skipped because of I/O errors (logged, not fatal).
    pub errors: AtomicU64,
    /// Dry-run only: hashes whose .links entry would be created by this
    /// run. The first file of a hash becomes the canonical entry and
    /// frees nothing; only subsequent duplicates count.
    dry_run_links: std::sync::Mutex<HashSet<String>>,
}

pub struct Options {
    pub store_dir: PathBuf,
    pub state_dir: PathBuf,
    pub dry_run: bool,
    /// Skip files below this size; the extra .links inode costs more.
    pub min_size: u64,
    pub jobs: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            store_dir: PathBuf::from("/nix/store"),
            state_dir: PathBuf::from("/nix/var/nix"),
            dry_run: false,
            min_size: 0,
            jobs: std::thread::available_parallelism().map_or(4, |n| n.get()),
        }
    }
}

/// Inodes already in `.links/`. Files with these inodes are already deduped
/// and can be skipped without hashing. Uses d_ino from readdir; with 1M+
/// link entries a stat per entry would dominate startup.
fn load_link_inodes(links_dir: &Path) -> Result<HashSet<u64>> {
    use nix::dir::Dir;
    use nix::fcntl::OFlag;
    use nix::sys::stat::Mode;

    let mut set = HashSet::default();
    let mut dir = match Dir::open(
        links_dir,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY,
        Mode::empty(),
    ) {
        Ok(d) => d,
        Err(nix::errno::Errno::ENOENT) => return Ok(set),
        Err(e) => return Err(e).with_context(|| format!("opening {}", links_dir.display())),
    };
    for entry in dir.iter() {
        let entry = entry.with_context(|| format!("reading {}", links_dir.display()))?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        set.insert(entry.ino());
    }
    Ok(set)
}

/// Replace `path` with a hardlink to `link_path`. Store dirs are read-only
/// with mtime 1; toggle writable around the rename and restore afterwards.
///
/// Holds a per-directory lock so concurrent tasks linking siblings don't
/// observe the dir flip back to read-only mid-operation.
fn replace_with_link(path: &Path, link_path: &Path, store_dir: &Path) -> Result<()> {
    let parent = path.parent().context("file has no parent")?;
    let must_toggle = parent != store_dir;

    let _dir_lock = if must_toggle {
        Some(dir_mutex(parent).lock().unwrap())
    } else {
        None
    };

    if must_toggle {
        // Store dirs are always r-xr-xr-x; no need to read first.
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755))?;
    }

    let restore = scopeguard(parent, must_toggle);

    // The temp link lives in the store root, like Nix's makeTempPath
    // (optimise-store.cc): a crash must leave the stray file *outside* the
    // store path, where the next GC removes it as unknown-on-disk. Inside
    // the path it would permanently corrupt the path's NAR contents.
    let tmp = store_dir.join(format!(
        ".tmp-link-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    fs::hard_link(link_path, &tmp)
        .with_context(|| format!("hardlink {} -> {}", link_path.display(), tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("rename {} -> {}", tmp.display(), path.display()));
    }
    drop(restore);
    Ok(())
}

/// Per-call random suffix for temp links. Concurrent tasks in this
/// process must not collide, so a global counter is mixed in.
fn rand_suffix() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = foldhash::fast::RandomState::default().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.finish()
}

struct ParentRestore<'a> {
    dir: &'a Path,
    active: bool,
}
impl Drop for ParentRestore<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = fs::set_permissions(self.dir, fs::Permissions::from_mode(0o555));
            let _ = set_mtime_to_one(self.dir);
        }
    }
}
fn scopeguard(dir: &Path, active: bool) -> ParentRestore<'_> {
    ParentRestore { dir, active }
}

/// Sharded mutex pool keyed by parent directory. Bounds memory while
/// keeping contention low for typical fan-out.
fn dir_mutex(dir: &Path) -> &'static std::sync::Mutex<()> {
    use std::hash::BuildHasher;
    use std::sync::OnceLock;
    const SHARDS: usize = 256;
    static POOL: OnceLock<Vec<std::sync::Mutex<()>>> = OnceLock::new();
    let pool = POOL.get_or_init(|| (0..SHARDS).map(|_| std::sync::Mutex::new(())).collect());
    let h = foldhash::fast::FixedState::default().hash_one(dir);
    &pool[h as usize % SHARDS]
}

fn set_mtime_to_one(path: &Path) -> std::io::Result<()> {
    use nix::sys::stat::{UtimensatFlags, utimensat};
    use nix::sys::time::TimeSpec;
    utimensat(
        nix::fcntl::AT_FDCWD,
        path,
        &TimeSpec::new(1, 0),
        &TimeSpec::new(1, 0),
        UtimensatFlags::NoFollowSymlink,
    )
    .map_err(std::io::Error::from)
}

async fn optimise_file(
    path: PathBuf,
    meta: fs::Metadata,
    links_dir: Arc<PathBuf>,
    store_dir: Arc<PathBuf>,
    opts: Arc<Options>,
    stats: Arc<Stats>,
) -> Result<()> {
    let ft = meta.file_type();
    // On macOS link(2) dereferences symlinks; only Linux can hardlink
    // them directly. Same gate as Nix's CAN_LINK_SYMLINK.
    let can_link_symlink = cfg!(target_os = "linux");
    let linkable = ft.is_file() || (ft.is_symlink() && can_link_symlink);
    if !linkable {
        return Ok(());
    }
    // HFS/APFS refuse hardlinks for some files inside .app bundles.
    // See https://github.com/NixOS/nix/issues/1443.
    #[cfg(target_os = "macos")]
    if path.to_str().is_some_and(|s| s.contains(".app/Contents/")) {
        return Ok(());
    }
    // Writable files in the store are suspect (e.g. fontconfig caches
    // mutated by root); skip them like nix does.
    if ft.is_file() && meta.mode() & 0o200 != 0 {
        log::warn!("skipping suspicious writable file {}", path.display());
        stats.files_skipped.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    if meta.len() < opts.min_size {
        stats.files_skipped.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }

    let hash = nar_hash_nix32(&path).await?;
    let link_path = links_dir.join(&hash);

    // symlink_metadata, not exists(): a dangling symlink in .links/
    // (corrupt state) would otherwise read as missing.
    let lmeta = match fs::symlink_metadata(&link_path) {
        Ok(m) => m,
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("lstat {}", link_path.display()));
        }
        Err(_) => {
            if opts.dry_run {
                // First occurrence becomes the canonical link (no rename,
                // nothing freed); only duplicates would be replaced.
                let first = stats.dry_run_links.lock().unwrap().insert(hash.clone());
                if !first {
                    stats.files_linked.fetch_add(1, Ordering::Relaxed);
                    stats.bytes_freed.fetch_add(meta.len(), Ordering::Relaxed);
                }
                return Ok(());
            }
            match fs::hard_link(&path, &link_path) {
                Ok(()) => {}
                // Lost a race to another worker.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                // Either the disk is full or the .links dir hit ext4's
                // directory index limit. Both are non-fatal: skip dedup for
                // this file. Logged once at the end to avoid one line per
                // file when the disk is genuinely full.
                Err(e) if e.raw_os_error() == Some(libc_enospc()) => {
                    stats.link_enospc.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                // Path GC'd between lstat and link.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("hardlink {} -> {}", path.display(), link_path.display())
                    });
                }
            }
            fs::symlink_metadata(&link_path)?
        }
    };
    if lmeta.ino() == meta.ino() {
        return Ok(());
    }
    // Size mismatch means a corrupt .links entry; don't merge.
    // Checked before the dry-run accounting: a real run skips these too.
    if lmeta.len() != meta.len() {
        log::warn!(
            "link {} has size {} but file {} has size {}; skipping",
            link_path.display(),
            lmeta.len(),
            path.display(),
            meta.len()
        );
        return Ok(());
    }
    if opts.dry_run {
        stats.files_linked.fetch_add(1, Ordering::Relaxed);
        stats.bytes_freed.fetch_add(meta.len(), Ordering::Relaxed);
        return Ok(());
    }

    let store_dir = store_dir.clone();
    let path2 = path.clone();
    let link_path2 = link_path.clone();
    let res =
        tokio::task::spawn_blocking(move || replace_with_link(&path2, &link_path2, &store_dir))
            .await?;
    match res {
        Ok(()) => {
            stats.files_linked.fetch_add(1, Ordering::Relaxed);
            stats.bytes_freed.fetch_add(meta.len(), Ordering::Relaxed);
        }
        Err(e) => {
            if let Some(io) = e.downcast_ref::<std::io::Error>() {
                // EMLINK: link count cap hit, typically empty files.
                if io.raw_os_error() == Some(libc_emlink()) {
                    if meta.len() > 0 {
                        log::info!(
                            "{} has reached maximum number of links",
                            link_path.display()
                        );
                    }
                    return Ok(());
                }
                // Path or parent dir GC'd while we worked on it.
                if io.kind() == std::io::ErrorKind::NotFound {
                    return Ok(());
                }
            }
            return Err(e);
        }
    }
    Ok(())
}

fn libc_enospc() -> i32 {
    nix::errno::Errno::ENOSPC as i32
}
fn libc_emlink() -> i32 {
    nix::errno::Errno::EMLINK as i32
}

pub async fn optimise_store(opts: Options) -> Result<Stats> {
    let links_dir = opts.store_dir.join(".links");
    if !opts.dry_run {
        make_store_writable(&opts.store_dir)?;
        fs::create_dir_all(&links_dir)
            .with_context(|| format!("creating {}", links_dir.display()))?;
    }

    // Like Nix's optimiseStore: register each path as a temp root right
    // before working on it instead of holding gc.lock shared for the
    // whole (potentially hours-long) run, which would block every GC.
    // Dry runs touch nothing and take no roots.
    let mut temp_roots = if opts.dry_run {
        None
    } else {
        Some(fast_nix_common::temp_roots::TempRoots::create(
            &opts.state_dir,
        )?)
    };

    let db = NixDb::open(&opts.store_dir, &opts.state_dir)?;
    let store_dir_typed: StoreDir = db.store_dir_typed()?;
    let paths = db.valid_store_paths()?;
    log::info!("optimising {} store paths", paths.len());

    let t0 = std::time::Instant::now();
    let ld = links_dir.clone();
    let known_inodes = Arc::new(tokio::task::spawn_blocking(move || load_link_inodes(&ld)).await??);
    log::info!(
        "loaded {} known link inodes in {:.1?}",
        known_inodes.len(),
        t0.elapsed()
    );

    let opts = Arc::new(opts);
    let links_dir = Arc::new(links_dir);
    let store_dir = Arc::new(opts.store_dir.clone());
    let stats = Arc::new(Stats::default());
    // Two independent pools. Sharing one would deadlock: walk tasks hold
    // permits while blocked on the bounded channel, starving consumers.
    let walk_sem = Arc::new(Semaphore::new(opts.jobs));
    let work_sem = Arc::new(Semaphore::new(opts.jobs * 2));
    let mut tasks: JoinSet<Result<()>> = JoinSet::new();

    let (file_tx, mut file_rx) =
        tokio::sync::mpsc::channel::<(PathBuf, fs::Metadata)>(opts.jobs * 16);

    let producer = {
        let known = known_inodes.clone();
        let stats = stats.clone();
        tokio::spawn(async move {
            let mut walks: JoinSet<Result<()>> = JoinSet::new();
            for store_path in paths {
                let permit = walk_sem.clone().acquire_owned().await?;
                let known = known.clone();
                let stats = stats.clone();
                let tx = file_tx.clone();
                let store_path = store_path.to_absolute_path(&store_dir_typed);
                if let Some(tr) = temp_roots.as_mut() {
                    let path_str = store_path.to_string_lossy();
                    tr.add(&path_str)
                        .with_context(|| format!("registering temp root {path_str}"))?;
                    // A GC running before our registration may have deleted
                    // the path (Nix: "path was GC'ed, probably").
                    if !db.is_valid_path(&path_str)? {
                        continue;
                    }
                }
                walks.spawn(async move {
                    let files = tokio::task::spawn_blocking(
                        move || -> Result<Vec<(PathBuf, fs::Metadata)>> {
                            use walkdir::DirEntryExt as _;
                            let mut out = Vec::new();
                            for entry in WalkDir::new(&store_path).follow_links(false) {
                                let entry = match entry {
                                    Ok(e) => e,
                                    // Path GC'd under us.
                                    Err(e)
                                        if e.io_error().is_some_and(|ioe| {
                                            ioe.kind() == std::io::ErrorKind::NotFound
                                        }) =>
                                    {
                                        continue;
                                    }
                                    Err(e) => return Err(e.into()),
                                };
                                let ft = entry.file_type();
                                if !ft.is_file() && !ft.is_symlink() {
                                    continue;
                                }
                                if known.contains(&entry.ino()) {
                                    continue;
                                }
                                // lstat once here; the per-file task needs
                                // size, mode, ino. d_type alone isn't enough.
                                let meta = match entry.metadata() {
                                    Ok(m) => m,
                                    Err(e)
                                        if e.io_error().is_some_and(|ioe| {
                                            ioe.kind() == std::io::ErrorKind::NotFound
                                        }) =>
                                    {
                                        continue;
                                    }
                                    Err(e) => return Err(e.into()),
                                };
                                out.push((entry.into_path(), meta));
                            }
                            Ok(out)
                        },
                    )
                    .await?;
                    // One unreadable path must not abort the whole run;
                    // log and dedup the rest, like nix-store --optimise.
                    let files = match files {
                        Ok(f) => f,
                        Err(e) => {
                            log::warn!("skipping store path: {e:#}");
                            stats.errors.fetch_add(1, Ordering::Relaxed);
                            drop(permit);
                            return Ok(());
                        }
                    };
                    // Hold the permit until the file list is drained:
                    // releasing it earlier lets new walks pile up while
                    // this one blocks on a full channel, accumulating
                    // store-wide metadata in memory. No deadlock: the
                    // consumer never takes walk permits.
                    for f in files {
                        if tx.send(f).await.is_err() {
                            break;
                        }
                    }
                    drop(permit);
                    Ok(())
                });
                while let Some(res) = walks.try_join_next() {
                    res??;
                }
            }
            while let Some(res) = walks.join_next().await {
                res??;
            }
            // Hand the temp roots back so they outlive the link workers,
            // not just the walks: dropping them here would release the
            // temproots flock while files are still being replaced.
            anyhow::Ok(temp_roots)
        })
    };

    while let Some((file, meta)) = file_rx.recv().await {
        let permit = work_sem.clone().acquire_owned().await?;
        let links_dir = links_dir.clone();
        let store_dir = store_dir.clone();
        let opts = opts.clone();
        let stats = stats.clone();
        tasks.spawn(async move {
            let stats2 = stats.clone();
            let _p = permit;
            if let Err(e) = optimise_file(file, meta, links_dir, store_dir, opts, stats).await {
                log::warn!("skipping file: {e:#}");
                stats2.errors.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        });
        while let Some(res) = tasks.try_join_next() {
            res??;
        }
    }

    let temp_roots = producer.await??;
    while let Some(res) = tasks.join_next().await {
        res??;
    }
    drop(temp_roots);

    Ok(Arc::into_inner(stats).expect("all tasks joined"))
}

pub fn cli_main() -> Result<()> {
    fast_nix_common::logging::init();

    let opts = parse_args()?;
    let dry = opts.dry_run;
    let jobs = opts.jobs;

    // Before tokio spawns its worker threads; see docs.
    if !dry {
        unshare_mount_namespace();
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(jobs)
        .enable_all()
        .build()
        .or_else(|e| {
            // e.g. EAGAIN when a sandbox limits thread creation
            log::warn!("failed to start {jobs} worker threads ({e}); falling back to one");
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
        })?;
    let stats = rt.block_on(optimise_store(opts))?;

    let enospc = stats.link_enospc.load(Ordering::Relaxed);
    if enospc > 0 {
        log::warn!("could not create {enospc} link(s): no space left on device");
    }
    let errors = stats.errors.load(Ordering::Relaxed);
    if errors > 0 {
        log::warn!("skipped {errors} file(s)/path(s) due to errors");
    }
    let skipped = stats.files_skipped.load(Ordering::Relaxed);
    if skipped > 0 {
        log::info!("{skipped} file(s) skipped (writable or below --min-size)");
    }
    let linked = stats.files_linked.load(Ordering::Relaxed);
    let freed = stats.bytes_freed.load(Ordering::Relaxed);
    if dry {
        println!(
            "{} would be freed by hard-linking {} files",
            format_size(freed),
            linked
        );
    } else {
        println!(
            "{} freed by hard-linking {} files",
            format_size(freed),
            linked
        );
    }
    Ok(())
}

fn parse_args() -> Result<Options> {
    let mut p = pico_args::Arguments::from_env();
    if p.contains("--help") {
        println!("Usage: fast-nix-optimise [OPTIONS]");
        println!();
        println!("Options:");
        println!("      --dry-run             Show what would be done");
        println!("      --min-size BYTES      Skip files smaller than BYTES");
        println!("  -j, --jobs N              Concurrency [default: num CPUs]");
        println!("      --store-dir PATH      Nix store directory [default: /nix/store]");
        println!("      --state-dir PATH      Nix state directory [default: /nix/var/nix]");
        std::process::exit(0);
    }

    let mut opts = Options {
        dry_run: p.contains("--dry-run"),
        ..Options::default()
    };
    if let Some(v) = p.opt_value_from_str("--min-size")? {
        opts.min_size = v;
    }
    if let Some(v) = p
        .opt_value_from_str("--jobs")?
        .or(p.opt_value_from_str("-j")?)
    {
        if v == 0 {
            bail!("--jobs must be at least 1");
        }
        opts.jobs = v;
    }
    if let Some(v) = p.opt_value_from_str("--store-dir")? {
        opts.store_dir = v;
    }
    if let Some(v) = p.opt_value_from_str("--state-dir")? {
        opts.state_dir = v;
    }
    let rest = p.finish();
    if let Some(first) = rest.first() {
        let arg = first.to_string_lossy();
        const KNOWN: &[&str] = &[
            "--dry-run",
            "--min-size",
            "-j",
            "--jobs",
            "--store-dir",
            "--state-dir",
            "--help",
        ];
        match fast_nix_common::closest_match(&arg, KNOWN) {
            Some(s) => bail!("unexpected argument '{arg}'; did you mean '{s}'?"),
            None => bail!("unexpected arguments: {rest:?} (see --help)"),
        }
    }
    Ok(opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dedups_identical_files() {
        let tmp = tempdir().unwrap();
        let store = tmp.path().join("store");
        let state = tmp.path().join("state");
        fs::create_dir_all(state.join("db")).unwrap();
        fs::create_dir_all(&store).unwrap();

        // Two store paths, each with an identical file.
        let p1 = store.join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a");
        let p2 = store.join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b");
        fs::create_dir_all(&p1).unwrap();
        fs::create_dir_all(&p2).unwrap();
        for p in [&p1, &p2] {
            let f = p.join("data");
            fs::write(&f, b"hello world hello world hello world\n").unwrap();
            fs::set_permissions(&f, fs::Permissions::from_mode(0o444)).unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o555)).unwrap();
        }

        // Minimal ValidPaths schema.
        let conn = rusqlite_open(&state.join("db/db.sqlite"));
        conn.execute_batch("CREATE TABLE ValidPaths (id INTEGER PRIMARY KEY, path TEXT NOT NULL);")
            .unwrap();
        for p in [&p1, &p2] {
            conn.execute(
                "INSERT INTO ValidPaths (path) VALUES (?)",
                [p.to_str().unwrap()],
            )
            .unwrap();
        }
        drop(conn);

        let stats = optimise_store(Options {
            store_dir: store.clone(),
            state_dir: state,
            jobs: 2,
            ..Options::default()
        })
        .await
        .unwrap();

        // First file *becomes* the canonical link (same inode, no rename),
        // so only the second one counts as "linked".
        assert_eq!(stats.files_linked.load(Ordering::Relaxed), 1);
        let i1 = fs::metadata(p1.join("data")).unwrap().ino();
        let i2 = fs::metadata(p2.join("data")).unwrap().ino();
        assert_eq!(i1, i2, "files should share an inode after optimise");
        assert!(store.join(".links").read_dir().unwrap().count() == 1);

        // Cleanup so tempdir can remove (dirs were made read-only).
        for p in [&p1, &p2] {
            fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn rusqlite_open(p: &Path) -> rusqlite::Connection {
        rusqlite::Connection::open(p).unwrap()
    }

    fn mk_store_path(store: &Path, name: &str, content: &[u8]) -> PathBuf {
        let p = store.join(name);
        fs::create_dir_all(&p).unwrap();
        let f = p.join("data");
        fs::write(&f, content).unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o555)).unwrap();
        p
    }

    fn mk_db(state: &Path, paths: &[&Path]) {
        fs::create_dir_all(state.join("db")).unwrap();
        let conn = rusqlite_open(&state.join("db/db.sqlite"));
        conn.execute_batch("CREATE TABLE ValidPaths (id INTEGER PRIMARY KEY, path TEXT NOT NULL);")
            .unwrap();
        for p in paths {
            conn.execute(
                "INSERT INTO ValidPaths (path) VALUES (?)",
                [p.to_str().unwrap()],
            )
            .unwrap();
        }
    }

    fn unlock(p: &Path) {
        fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn run_opts(store: &Path, state: &Path) -> Options {
        Options {
            store_dir: store.to_path_buf(),
            state_dir: state.to_path_buf(),
            jobs: 1,
            ..Options::default()
        }
    }

    #[test]
    fn load_link_inodes_missing_dir_is_empty() {
        let tmp = tempdir().unwrap();
        let set = load_link_inodes(&tmp.path().join("nope")).unwrap();
        assert!(set.is_empty());
    }

    #[test]
    fn load_link_inodes_returns_actual_inodes() {
        let tmp = tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        fs::write(&a, b"x").unwrap();
        fs::write(&b, b"y").unwrap();
        let set = load_link_inodes(tmp.path()).unwrap();
        assert_eq!(set.len(), 2);
        assert!(set.contains(&fs::metadata(&a).unwrap().ino()));
        assert!(set.contains(&fs::metadata(&b).unwrap().ino()));
    }

    #[test]
    fn rand_suffix_is_unique_per_call() {
        assert_ne!(rand_suffix(), rand_suffix());
    }

    #[test]
    fn errno_helpers_match_libc_values() {
        assert_eq!(libc_enospc(), nix::errno::Errno::ENOSPC as i32);
        assert_eq!(libc_emlink(), nix::errno::Errno::EMLINK as i32);
        assert!(libc_enospc() > 1);
        assert!(libc_emlink() > 1);
        assert_ne!(libc_enospc(), libc_emlink());
    }

    const CONTENT: &[u8] = b"hello world hello world hello world\n";

    async fn run_two_identical(
        min_size: u64,
        dry_run: bool,
    ) -> (Stats, PathBuf, PathBuf, tempfile::TempDir) {
        let tmp = tempdir().unwrap();
        let store = tmp.path().join("store");
        let state = tmp.path().join("state");
        fs::create_dir_all(&store).unwrap();
        let p1 = mk_store_path(&store, "cccccccccccccccccccccccccccccccc-c", CONTENT);
        let p2 = mk_store_path(&store, "dddddddddddddddddddddddddddddddd-d", CONTENT);
        mk_db(&state, &[&p1, &p2]);
        let stats = optimise_store(Options {
            min_size,
            dry_run,
            ..run_opts(&store, &state)
        })
        .await
        .unwrap();
        unlock(&p1);
        unlock(&p2);
        (stats, p1.join("data"), p2.join("data"), tmp)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn min_size_equal_to_len_is_processed() {
        let (stats, f1, f2, _tmp) = run_two_identical(CONTENT.len() as u64, false).await;
        assert_eq!(stats.files_linked.load(Ordering::Relaxed), 1);
        assert_eq!(stats.files_skipped.load(Ordering::Relaxed), 0);
        assert_eq!(
            fs::metadata(&f1).unwrap().ino(),
            fs::metadata(&f2).unwrap().ino()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn min_size_above_len_is_skipped() {
        let (stats, f1, f2, _tmp) = run_two_identical(CONTENT.len() as u64 + 1, false).await;
        assert_eq!(stats.files_linked.load(Ordering::Relaxed), 0);
        assert_eq!(stats.files_skipped.load(Ordering::Relaxed), 2);
        assert_ne!(
            fs::metadata(&f1).unwrap().ino(),
            fs::metadata(&f2).unwrap().ino()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dry_run_counts_but_does_not_link() {
        let (stats, f1, f2, _tmp) = run_two_identical(0, true).await;
        // The first file would become the canonical .links entry and
        // frees nothing; only the duplicate counts — matching a real run.
        assert_eq!(stats.files_linked.load(Ordering::Relaxed), 1);
        assert_eq!(
            stats.bytes_freed.load(Ordering::Relaxed),
            CONTENT.len() as u64
        );
        assert_ne!(
            fs::metadata(&f1).unwrap().ino(),
            fs::metadata(&f2).unwrap().ino()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restores_parent_perms_and_mtime() {
        let tmp = tempdir().unwrap();
        let store = tmp.path().join("store");
        let state = tmp.path().join("state");
        fs::create_dir_all(&store).unwrap();
        let p1 = mk_store_path(&store, "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk-k", CONTENT);
        let p2 = mk_store_path(&store, "ffffffffffffffffffffffffffffffff-f", CONTENT);
        mk_db(&state, &[&p1, &p2]);
        let stats = optimise_store(run_opts(&store, &state)).await.unwrap();
        assert_eq!(stats.files_linked.load(Ordering::Relaxed), 1);
        // Exactly one dir had its file replaced; that dir must be restored
        // to mode 0555 with mtime 1.
        let restored: Vec<_> = [&p1, &p2]
            .into_iter()
            .map(|p| fs::metadata(p).unwrap())
            .filter(|m| m.mtime() == 1)
            .collect();
        assert_eq!(restored.len(), 1, "one parent dir restored to mtime 1");
        assert_eq!(restored[0].permissions().mode() & 0o777, 0o555);
        unlock(&p1);
        unlock(&p2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn corrupt_link_with_size_mismatch_is_skipped() {
        let tmp = tempdir().unwrap();
        let store = tmp.path().join("store");
        let state = tmp.path().join("state");
        fs::create_dir_all(&store).unwrap();
        let p1 = mk_store_path(&store, "gggggggggggggggggggggggggggggggg-g", CONTENT);
        mk_db(&state, &[&p1]);
        // Pre-create a corrupt .links entry under the file's hash with a
        // different size.
        let hash = nar_hash_nix32(&p1.join("data")).await.unwrap();
        let links = store.join(".links");
        fs::create_dir_all(&links).unwrap();
        fs::write(links.join(&hash), b"wrong size").unwrap();

        let stats = optimise_store(run_opts(&store, &state)).await.unwrap();
        assert_eq!(stats.files_linked.load(Ordering::Relaxed), 0);
        assert_ne!(
            fs::metadata(p1.join("data")).unwrap().ino(),
            fs::metadata(links.join(&hash)).unwrap().ino()
        );
        unlock(&p1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canonical_file_is_not_counted_as_linked() {
        let tmp = tempdir().unwrap();
        let store = tmp.path().join("store");
        let state = tmp.path().join("state");
        fs::create_dir_all(&store).unwrap();
        let p1 = mk_store_path(&store, "hhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhh-h", CONTENT);
        mk_db(&state, &[&p1]);
        let stats = optimise_store(run_opts(&store, &state)).await.unwrap();
        // Sole file becomes the canonical .links entry (same inode);
        // nothing is replaced, so the counter must stay 0.
        assert_eq!(stats.files_linked.load(Ordering::Relaxed), 0);
        assert_eq!(store.join(".links").read_dir().unwrap().count(), 1);
        unlock(&p1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writable_file_is_skipped_as_suspicious() {
        let tmp = tempdir().unwrap();
        let store = tmp.path().join("store");
        let state = tmp.path().join("state");
        fs::create_dir_all(&store).unwrap();
        let p1 = mk_store_path(&store, "iiiiiiiiiiiiiiiiiiiiiiiiiiiiiiii-i", CONTENT);
        let p2 = mk_store_path(&store, "jjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjj-j", CONTENT);
        unlock(&p2);
        fs::set_permissions(p2.join("data"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&p2, fs::Permissions::from_mode(0o555)).unwrap();
        mk_db(&state, &[&p1, &p2]);
        let stats = optimise_store(run_opts(&store, &state)).await.unwrap();
        assert_eq!(stats.files_linked.load(Ordering::Relaxed), 0);
        assert_eq!(stats.files_skipped.load(Ordering::Relaxed), 1);
        unlock(&p1);
        unlock(&p2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registers_temp_roots_for_optimised_paths() {
        let tmp = tempdir().unwrap();
        let store = tmp.path().join("store");
        let state = tmp.path().join("state");
        fs::create_dir_all(&store).unwrap();
        let p1 = mk_store_path(&store, "qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq-q", CONTENT);
        let p2 = mk_store_path(&store, "rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr-r", CONTENT);
        mk_db(&state, &[&p1, &p2]);

        optimise_store(run_opts(&store, &state)).await.unwrap();

        // Every optimised path was registered in our temproots file, so a
        // concurrent GC would not have deleted it mid-replace.
        let roots = fs::read(state.join("temproots").join(std::process::id().to_string())).unwrap();
        let roots = String::from_utf8(roots).unwrap();
        assert!(roots.contains(p1.to_str().unwrap()), "{roots}");
        assert!(roots.contains(p2.to_str().unwrap()), "{roots}");
        unlock(&p1);
        unlock(&p2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dry_run_takes_no_temp_roots() {
        let (_stats, _f1, _f2, tmp) = run_two_identical(0, true).await;
        let tr = tmp
            .path()
            .join("state/temproots")
            .join(std::process::id().to_string());
        assert!(!tr.exists(), "dry run must not write temp roots");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unreadable_path_is_skipped_not_fatal() {
        if nix::unistd::geteuid().is_root() {
            return; // root bypasses permission checks
        }
        let tmp = tempdir().unwrap();
        let store = tmp.path().join("store");
        let state = tmp.path().join("state");
        fs::create_dir_all(&store).unwrap();
        let bad = mk_store_path(&store, "llllllllllllllllllllllllllllllll-l", CONTENT);
        let p1 = mk_store_path(&store, "mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm-m", CONTENT);
        let p2 = mk_store_path(&store, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn-n", CONTENT);
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();
        mk_db(&state, &[&bad, &p1, &p2]);

        let stats = optimise_store(run_opts(&store, &state)).await.unwrap();

        // The unreadable path is skipped; the other two still dedup.
        assert_eq!(stats.errors.load(Ordering::Relaxed), 1);
        assert_eq!(stats.files_linked.load(Ordering::Relaxed), 1);
        unlock(&bad);
        unlock(&p1);
        unlock(&p2);
    }
}
