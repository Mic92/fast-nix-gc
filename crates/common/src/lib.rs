pub mod db;
pub mod logging;

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;
use std::path::Path;

/// Enter a private mount namespace so the rw remount in
/// `make_store_writable` is scoped to this process and does not leak into
/// the system, mirroring what the `nix` CLI does for root.
///
/// Must be called from `main` before any thread pool is spawned:
/// `unshare(CLONE_NEWNS)` only moves the calling thread into the new
/// namespace, so worker threads created earlier would stay in the host
/// namespace and not see the remount.
///
/// Failures are logged and ignored (e.g. EPERM in containers); we then
/// fall back to remounting in the host namespace, like legacy `nix-store`.
#[cfg(target_os = "linux")]
pub fn unshare_mount_namespace() {
    use nix::mount::{MsFlags, mount};
    use nix::sched::{CloneFlags, unshare};
    use nix::unistd::Uid;

    if !Uid::effective().is_root() {
        return;
    }
    if let Err(e) = unshare(CloneFlags::CLONE_NEWNS) {
        log::warn!("failed to set up a private mount namespace: {e}");
        return;
    }
    // Default propagation on systemd is `shared`; without this a remount
    // would propagate back to the host namespace.
    if let Err(e) = mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_PRIVATE | MsFlags::MS_REC,
        None::<&str>,
    ) {
        log::warn!("failed to mark / private in mount namespace: {e}");
    }
}

#[cfg(not(target_os = "linux"))]
pub fn unshare_mount_namespace() {}

/// NixOS bind-mounts /nix/store read-only; remount rw before mutating.
#[cfg(target_os = "linux")]
pub fn make_store_writable(real_store_dir: &Path) -> Result<()> {
    use nix::mount::{MsFlags, mount};
    use nix::sys::statvfs::{FsFlags, statvfs};
    use nix::unistd::Uid;

    if !Uid::effective().is_root() {
        return Ok(());
    }

    let st = statvfs(real_store_dir).context("getting Nix store mount info")?;
    if !st.flags().contains(FsFlags::ST_RDONLY) {
        return Ok(());
    }

    // Preserve locked mount flags (nodev etc.) or remount fails in a userns.
    let mut flags = MsFlags::MS_REMOUNT | MsFlags::MS_BIND;
    let f = st.flags();
    for (fs_flag, ms_flag) in [
        (FsFlags::ST_NODEV, MsFlags::MS_NODEV),
        (FsFlags::ST_NOSUID, MsFlags::MS_NOSUID),
        (FsFlags::ST_NOEXEC, MsFlags::MS_NOEXEC),
        (FsFlags::ST_NOATIME, MsFlags::MS_NOATIME),
        (FsFlags::ST_NODIRATIME, MsFlags::MS_NODIRATIME),
        (FsFlags::ST_RELATIME, MsFlags::MS_RELATIME),
        (FsFlags::ST_SYNCHRONOUS, MsFlags::MS_SYNCHRONOUS),
    ] {
        if f.contains(fs_flag) {
            flags |= ms_flag;
        }
    }

    mount(
        None::<&str>,
        real_store_dir,
        None::<&str>,
        flags,
        None::<&str>,
    )
    .with_context(|| format!("remounting {} writable", real_store_dir.display()))?;
    log::info!("remounted {} read-write", real_store_dir.display());
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn make_store_writable(_real_store_dir: &Path) -> Result<()> {
    Ok(())
}

// foldhash: SipHash showed up in profiles hashing 50-char store paths.
pub type HashMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;
pub type HashSet<K> = std::collections::HashSet<K, foldhash::fast::RandomState>;

pub fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.2} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.2} KiB", b / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::format_size;

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(1023), "1023 bytes");
        assert_eq!(format_size(1024), "1.00 KiB");
        assert_eq!(format_size(1536), "1.50 KiB");
        assert_eq!(format_size(1024 * 1024), "1.00 MiB");
        assert_eq!(format_size(5 * 1024 * 1024 + 512 * 1024), "5.50 MiB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.00 GiB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024 / 2), "1.50 GiB");
    }
}
pub mod nix_config;
pub mod temp_roots;

/// Closest CLI flag by edit distance, for "did you mean" hints.
/// Returns None unless a candidate is within distance 3.
pub fn closest_match<'a>(arg: &str, candidates: &[&'a str]) -> Option<&'a str> {
    fn edit_distance(a: &str, b: &str) -> usize {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        let mut cur = vec![0; b.len() + 1];
        for i in 1..=a.len() {
            cur[0] = i;
            for j in 1..=b.len() {
                let sub = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
                cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
            }
            std::mem::swap(&mut prev, &mut cur);
        }
        prev[b.len()]
    }
    candidates
        .iter()
        .map(|c| (edit_distance(arg, c), *c))
        .filter(|&(d, _)| d <= 3)
        .min()
        .map(|(_, c)| c)
}

#[cfg(test)]
mod closest_match_tests {
    use super::closest_match;

    #[test]
    fn suggests_near_misses_only() {
        let known = &["--dry-run", "--keep-recent", "--store-dir"];
        assert_eq!(closest_match("--keep-resent", known), Some("--keep-recent"));
        assert_eq!(closest_match("--dry-rnu", known), Some("--dry-run"));
        assert_eq!(closest_match("--completely-different", known), None);
    }
}
