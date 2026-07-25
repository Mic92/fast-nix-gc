use anyhow::{Context, Result, bail};
use fast_nix_common::unshare_mount_namespace;
use fast_nix_gc::{db, format_size, gc, profiles};
use rayon::prelude::*;
use std::path::{Path, PathBuf};

struct Args {
    delete_old: bool,
    delete_older_than: Option<String>,
    dry_run: bool,
    ensure_free: Option<EnsureFree>,
    keep_recent: Option<String>,
    no_vacuum: bool,
    chunk_size: Option<usize>,
    keep_outputs: Option<bool>,
    keep_derivations: Option<bool>,
    gc_roots_dirs: Vec<PathBuf>,
    store_dir: PathBuf,
    state_dir: PathBuf,
}

/// `--ensure-free` target: absolute bytes or a percentage of the store's
/// filesystem.
#[derive(Debug, Clone, Copy, PartialEq)]
enum EnsureFree {
    Bytes(u64),
    Percent(f64),
}

impl EnsureFree {
    fn target_bytes(self, total: u64) -> u64 {
        match self {
            EnsureFree::Bytes(b) => b,
            EnsureFree::Percent(p) => (total as f64 * p / 100.0) as u64,
        }
    }
}

/// Parse a size like "50G" or a percentage like "20%".
fn parse_ensure_free(s: &str) -> Result<EnsureFree> {
    let s = s.trim();
    if let Some(num) = s.strip_suffix('%') {
        let p: f64 = num
            .trim()
            .parse()
            .with_context(|| format!("invalid percentage '{s}'"))?;
        if !p.is_finite() || !(0.0..=100.0).contains(&p) {
            bail!("percentage '{s}' must be between 0 and 100");
        }
        Ok(EnsureFree::Percent(p))
    } else {
        Ok(EnsureFree::Bytes(parse_size(s)?))
    }
}

/// Parse a size like "50G", "512M", "1024K", or plain bytes.
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1024u64),
        Some('M' | 'm') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G' | 'g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        Some('T' | 't') => (&s[..s.len() - 1], 1024u64.pow(4)),
        _ => (s, 1),
    };
    let n: f64 = num.parse().with_context(|| format!("invalid size '{s}'"))?;
    if !n.is_finite() || n < 0.0 || n * mult as f64 > u64::MAX as f64 {
        bail!("size '{s}' is out of range");
    }
    Ok((n * mult as f64) as u64)
}

/// Free and total bytes on the filesystem containing `path`.
fn filesystem_bytes(path: &Path) -> Result<(u64, u64)> {
    let st =
        nix::sys::statvfs::statvfs(path).with_context(|| format!("statvfs {}", path.display()))?;
    // statvfs field types differ between Linux (u64) and macOS (u32).
    let frag = st.fragment_size() as u64;
    let avail = st.blocks_available() as u64 * frag;
    let total = st.blocks() as u64 * frag;
    Ok((avail, total))
}

fn parse_args() -> Result<Args> {
    parse_args_from(std::env::args_os().skip(1).collect())
}

fn parse_args_from(args: Vec<std::ffi::OsString>) -> Result<Args> {
    let mut pargs = pico_args::Arguments::from_vec(args);

    if pargs.contains("--help") {
        println!("Usage: fast-nix-gc [OPTIONS]");
        println!();
        println!("Options:");
        println!("  -d, --delete-old              Remove old profile generations");
        println!("      --delete-older-than SPEC  Delete generations older than SPEC (e.g. 30d)");
        println!("      --dry-run                 Show what would be done");
        println!("      --ensure-free SIZE        Free until SIZE is available (e.g. 50G or 20%)");
        println!("      --keep-recent SPEC        Keep paths registered within SPEC (e.g. 7d)");
        println!("                                (pinned paths are never freed by --ensure-free)");
        println!("      --no-vacuum               Skip the database VACUUM after deletion");
        println!(
            "      --chunk-size N            Dead paths per delete transaction [default: 65536]"
        );
        println!("      --keep-outputs BOOL       Override the keep-outputs nix.conf setting");
        println!("      --keep-derivations BOOL   Override the keep-derivations nix.conf setting");
        println!(
            "      --gc-roots-dir PATH       Extra directory to scan for GC roots (repeatable)"
        );
        println!("      --store-dir PATH          Nix store directory [default: /nix/store]");
        println!("      --state-dir PATH          Nix state directory [default: /nix/var/nix]");
        std::process::exit(0);
    }

    let delete_older_than: Option<String> = pargs.opt_value_from_str("--delete-older-than")?;
    let delete_old =
        pargs.contains("-d") || pargs.contains("--delete-old") || delete_older_than.is_some();

    let args = Args {
        delete_old,
        delete_older_than,
        dry_run: pargs.contains("--dry-run"),
        ensure_free: pargs.opt_value_from_fn("--ensure-free", parse_ensure_free)?,
        keep_recent: pargs.opt_value_from_str("--keep-recent")?,
        no_vacuum: pargs.contains("--no-vacuum"),
        chunk_size: pargs.opt_value_from_str("--chunk-size")?,
        keep_outputs: pargs.opt_value_from_str("--keep-outputs")?,
        keep_derivations: pargs.opt_value_from_str("--keep-derivations")?,
        gc_roots_dirs: pargs.values_from_str("--gc-roots-dir")?,
        store_dir: pargs
            .opt_value_from_str("--store-dir")?
            .unwrap_or_else(|| PathBuf::from("/nix/store")),
        state_dir: pargs
            .opt_value_from_str("--state-dir")?
            .unwrap_or_else(|| PathBuf::from("/nix/var/nix")),
    };
    // A typo'd flag (e.g. --dry-rnu) must not silently run a destructive GC.
    let rest = pargs.finish();
    if let Some(first) = rest.first() {
        let arg = first.to_string_lossy();
        const KNOWN: &[&str] = &[
            "-d",
            "--delete-old",
            "--delete-older-than",
            "--dry-run",
            "--ensure-free",
            "--keep-recent",
            "--no-vacuum",
            "--chunk-size",
            "--keep-outputs",
            "--keep-derivations",
            "--gc-roots-dir",
            "--store-dir",
            "--state-dir",
            "--help",
        ];
        match fast_nix_common::closest_match(&arg, KNOWN) {
            Some(s) => bail!("unexpected argument '{arg}'; did you mean '{s}'?"),
            None => bail!("unexpected arguments: {rest:?} (see --help)"),
        }
    }
    Ok(args)
}

/// Initialize the rayon global pool up front so thread creation failures
/// (e.g. EAGAIN under a sandbox thread limit) fall back to a single thread
/// instead of panicking on the first `par_iter`.
fn init_rayon() {
    if let Err(e) = rayon::ThreadPoolBuilder::new().build_global() {
        log::warn!("failed to start rayon thread pool ({e}); falling back to a single thread");
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build_global();
    }
}

fn main() -> Result<()> {
    fast_nix_common::logging::init();

    let args = parse_args()?;

    // Must happen before rayon spawns its worker threads: only the calling
    // thread joins the new mount namespace, so threads created earlier would
    // still see the read-only /nix/store.
    if !args.dry_run {
        unshare_mount_namespace();
    }
    init_rayon();

    if args.ensure_free.is_some() && args.dry_run {
        bail!("--ensure-free cannot be combined with --dry-run");
    }

    // Validate every time spec before any destructive work; a bad
    // --keep-recent must not surface only after generations were deleted.
    let delete_older_cutoff = args
        .delete_older_than
        .as_deref()
        .map(profiles::parse_older_than)
        .transpose()?;
    let keep_recent_after = args
        .keep_recent
        .as_deref()
        .map(profiles::parse_older_than)
        .transpose()?
        .map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });

    if args.delete_old {
        profiles::profile_dirs(&args.state_dir)
            .par_iter()
            .try_for_each(|dir| {
                profiles::remove_old_generations(dir, delete_older_cutoff, args.dry_run)
            })?;
    }

    let max_freed = if let Some(ensure_free) = args.ensure_free {
        let (avail, total) = filesystem_bytes(&args.store_dir)?;
        let target = ensure_free.target_bytes(total);
        if avail >= target {
            println!("{} already free, nothing to do", format_size(avail));
            return Ok(());
        }
        let need = target - avail;
        log::info!(
            "freeing at least {} to reach {} free",
            format_size(need),
            format_size(target)
        );
        Some(need)
    } else {
        None
    };
    let ensure_free_need = max_freed;

    let mut store = if args.dry_run {
        // No DB writes happen in a dry run; don't take write locks or
        // flip the journal mode. A WAL database without its -shm/-wal
        // sidecars can't be opened read-only, so fall back to read-write.
        db::NixDb::open_read_only(&args.store_dir, &args.state_dir).or_else(|e| {
            log::debug!("read-only open failed ({e:#}); retrying read-write");
            db::NixDb::open(&args.store_dir, &args.state_dir)
        })?
    } else {
        db::NixDb::open(&args.store_dir, &args.state_dir)?
    };
    if let Some(v) = args.keep_outputs {
        store.keep_outputs = v;
    }
    if let Some(v) = args.keep_derivations {
        store.keep_derivations = v;
    }
    let opts = gc::GcOptions {
        dry_run: args.dry_run,
        max_freed,
        keep_recent_after,
        no_vacuum: args.no_vacuum,
        chunk_size: args.chunk_size,
        extra_gc_roots_dirs: args.gc_roots_dirs,
    };
    let (bytes_freed, paths_deleted) = gc::collect_garbage(&store, &opts)?;

    if let Some(need) = ensure_free_need {
        if bytes_freed < need {
            let hint = if keep_recent_after.is_some() {
                " (reachable from GC roots or pinned by --keep-recent)"
            } else {
                " (reachable from GC roots)"
            };
            log::warn!(
                "only freed {}, {} short of --ensure-free target; \
                 remaining paths are alive{hint}",
                format_size(bytes_freed),
                format_size(need - bytes_freed)
            );
        }
    }

    if args.dry_run {
        println!(
            "{paths_deleted} store paths would be deleted (~{})",
            format_size(bytes_freed)
        );
    } else {
        println!(
            "{paths_deleted} store paths deleted, {} freed",
            format_size(bytes_freed)
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{EnsureFree, parse_args_from, parse_ensure_free, parse_size};

    fn args(list: &[&str]) -> Vec<std::ffi::OsString> {
        list.iter().map(|s| s.into()).collect()
    }

    #[test]
    fn parse_args_rejects_unknown_arguments() {
        let err = parse_args_from(args(&["--dry-rnu"])).err().unwrap();
        assert!(err.to_string().contains("--dry-run"), "{err}");
        let err = parse_args_from(args(&["--keep-resent", "2d"]))
            .err()
            .unwrap();
        assert!(err.to_string().contains("--keep-recent"), "{err}");
        assert!(parse_args_from(args(&["--dry-run", "extra"])).is_err());
        let parsed = parse_args_from(args(&["--dry-run"])).unwrap();
        assert!(parsed.dry_run);
    }

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("123").unwrap(), 123);
        assert_eq!(parse_size("1K").unwrap(), 1024);
        assert_eq!(parse_size("2k").unwrap(), 2048);
        assert_eq!(parse_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("3m").unwrap(), 3 * 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("2g").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("1T").unwrap(), 1024u64.pow(4));
        assert_eq!(parse_size("1.5K").unwrap(), 1536);
        assert_eq!(parse_size(" 4M ").unwrap(), 4 * 1024 * 1024);
        assert!(parse_size("abc").is_err());
        assert!(parse_size("-5G").is_err());
        assert!(parse_size("inf").is_err());
        assert!(parse_size("NaN").is_err());
        assert!(parse_size("99999999999999999999G").is_err());
    }

    #[test]
    fn parse_ensure_free_values() {
        assert_eq!(
            parse_ensure_free("50G").unwrap(),
            EnsureFree::Bytes(50 * 1024u64.pow(3))
        );
        assert_eq!(parse_ensure_free("1024").unwrap(), EnsureFree::Bytes(1024));
        assert_eq!(parse_ensure_free("20%").unwrap(), EnsureFree::Percent(20.0));
        assert_eq!(
            parse_ensure_free(" 12.5 % ").unwrap(),
            EnsureFree::Percent(12.5)
        );
        assert_eq!(
            parse_ensure_free("100%").unwrap(),
            EnsureFree::Percent(100.0)
        );
        assert!(parse_ensure_free("101%").is_err());
        assert!(parse_ensure_free("-5%").is_err());
        assert!(parse_ensure_free("abc%").is_err());
    }
}
