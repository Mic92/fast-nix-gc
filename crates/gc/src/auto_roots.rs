//! Stale gcroots/auto pruning (--delete-older-than).
//!
//! `gcroots/auto` accumulates indirect roots forever: nix-direnv devshells
//! of abandoned projects and old `nix build` result links keep their
//! closures alive until the user hunts them down by hand. When the user
//! already asked for age-based cleanup via --delete-older-than, prune these
//! too:
//!
//! - devshell roots (target under a `.direnv/` directory): stale when the
//!   newest atime/mtime across the project's `.direnv/*.rc` files is older
//!   than the cutoff. nix-direnv sources the cached rc on every load, so
//!   its atime tracks the last load (day granularity under relatime; under
//!   noatime this degrades to the last rebuild). A `.direnv` without rc
//!   files falls back to the auto link's own mtime.
//! - all other auto roots: stale when the auto link's own mtime (= the
//!   moment Nix registered the root) is older than the cutoff.
//!
//! Removing the auto link only unroots: the store paths are collected by
//! the GC pass that follows, and nix-direnv re-registers on the next visit
//! to a pruned project. Dangling links are not our job — root discovery
//! removes them (see roots::remove_stale_auto_link).

use anyhow::{Context, Result};
use std::fs;
use std::path::{Component, Path};
use std::time::SystemTime;

/// Prune symlinks in `<state_dir>/gcroots/auto` whose last use predates
/// `cutoff`. `prune_devshell` / `prune_other` correspond to the
/// `--no-prune-devshell-roots` / `--no-prune-auto-roots` opt-outs.
pub fn prune_stale_auto_roots(
    state_dir: &Path,
    cutoff: SystemTime,
    prune_devshell: bool,
    prune_other: bool,
    dry_run: bool,
) -> Result<()> {
    let auto = state_dir.join("gcroots/auto");
    let entries = match fs::read_dir(&auto) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("opening {}", auto.display())),
    };

    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", auto.display()))?;
        let link = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_symlink()) {
            continue;
        }
        let Ok(target) = fs::read_link(&link) else {
            continue;
        };
        // Dangling roots are removed during root discovery, not here.
        if fs::symlink_metadata(&target).is_err() {
            continue;
        }

        let (last_used, kind) = match direnv_dir(&target) {
            Some(direnv) => {
                if !prune_devshell {
                    continue;
                }
                match newest_rc_activity(&direnv) {
                    Some(t) => (t, "unused devshell"),
                    // No cached rc to judge by; the link's registration
                    // time is the only signal left.
                    None => match link_mtime(&link) {
                        Some(t) => (t, "unused devshell"),
                        None => continue,
                    },
                }
            }
            None => {
                if !prune_other {
                    continue;
                }
                match link_mtime(&link) {
                    Some(t) => (t, "old root"),
                    None => continue,
                }
            }
        };

        if last_used >= cutoff {
            continue;
        }
        if dry_run {
            println!("would remove {}: {}", kind, target.display());
        } else {
            log::info!("removing {}: {}", kind, target.display());
            fs::remove_file(&link).with_context(|| format!("removing {}", link.display()))?;
        }
    }
    Ok(())
}

/// If `target` lies inside a `.direnv` directory, return the path of that
/// `.direnv` directory. Component match, not substring: a project named
/// "foo.direnv" must not qualify.
fn direnv_dir(target: &Path) -> Option<std::path::PathBuf> {
    let mut dir = std::path::PathBuf::new();
    for comp in target.components() {
        dir.push(comp);
        if matches!(comp, Component::Normal(name) if name == ".direnv") {
            return Some(dir);
        }
    }
    None
}

/// Newest atime/mtime across the `.direnv/*.rc` files, i.e. the last time
/// nix-direnv loaded (atime) or rebuilt (mtime) the devshell.
fn newest_rc_activity(direnv: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    for entry in fs::read_dir(direnv).ok()?.flatten() {
        if entry.path().extension().is_none_or(|e| e != "rc") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        for t in [meta.accessed().ok(), meta.modified().ok()] {
            if let Some(t) = t
                && newest.is_none_or(|n| t > n)
            {
                newest = Some(t);
            }
        }
    }
    newest
}

fn link_mtime(link: &Path) -> Option<SystemTime> {
    fs::symlink_metadata(link).ok()?.modified().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direnv_dir_matches_component_only() {
        assert_eq!(
            direnv_dir(Path::new("/p/x/.direnv/flake-profile-1-link")),
            Some(std::path::PathBuf::from("/p/x/.direnv"))
        );
        assert_eq!(direnv_dir(Path::new("/p/x.direnv/link")), None);
        assert_eq!(direnv_dir(Path::new("/p/x/result")), None);
    }
}
