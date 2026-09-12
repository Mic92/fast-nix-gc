# fast-nix-gc

Faster `nix-collect-garbage` and `nix-store --optimise`. See
[real-world timings](#real-world-timings) for a rough idea of how much
faster.

## fast-nix-gc

The stock GC issues one SQLite query per store path while traversing the
reference graph. With ~100K paths this means ~100K B-tree seeks and a lot
of statement-cache churn. fast-nix-gc instead reads `ValidPaths` and `Refs`
once into a CSR adjacency list and runs the liveness BFS over integer node
ids. On a real store with ~30K dead paths this brings the dry-run from
~20s down to ~1s. Disk deletion and `.links` cleanup are parallelized
with rayon.

```
fast-nix-gc [OPTIONS]

  -d, --delete-old              Remove old profile generations
      --delete-older-than SPEC  Delete generations and stale gcroots/auto
                                entries older than SPEC (e.g. 30d, 4h)
      --dry-run                 Show what would be done
      --ensure-free SIZE        Free until SIZE is available (e.g. 50G or 20%)
      --keep-recent SPEC        Keep paths registered within SPEC (e.g. 1d)
      --no-vacuum               Skip the database VACUUM after deletion
      --no-prune-devshell-roots Keep stale nix-direnv devshell GC roots
      --no-prune-auto-roots     Keep other stale gcroots/auto links
      --chunk-size N            Dead paths per delete transaction [default: 65536]
      --keep-outputs BOOL       Override the keep-outputs nix.conf setting
      --keep-derivations BOOL   Override the keep-derivations nix.conf setting
      --gc-roots-dir PATH       Extra directory to scan for GC roots (repeatable)
      --store-dir PATH          Nix store directory [default: /nix/store]
      --state-dir PATH          Nix state directory [default: /nix/var/nix]
```

### Stale GC roots

`gcroots/auto` accumulates indirect roots forever: nix-direnv devshells of
abandoned projects and forgotten `nix build` result links pin their whole
closures. With `--delete-older-than`, fast-nix-gc also prunes these:

- **devshell roots** (targets under a `.direnv/` directory) go when the
  shell was not *loaded* within the window, judged by the newest
  atime/mtime of the cached `.direnv/*.rc` — nix-direnv sources it on
  every load, so relatime keeps the atime current to the day. On
  `noatime` mounts the signal degrades to the last rebuild. Opt out with
  `--no-prune-devshell-roots`.
- **all other auto roots** go when the link itself is older than the
  window (its mtime is the moment Nix registered the root). Opt out with
  `--no-prune-auto-roots`.

Pruning only unroots: the closure is collected by the same run, the
project's `result` / `.direnv` symlinks are left in place (now dangling),
and nix-direnv re-registers on the next visit, at worst rebuilding the
shell.

## fast-nix-optimise

Hardlink-based store dedup, on-disk compatible with `nix-store --optimise`:
It uses the same `.links/` layout and the same NAR-SHA-256 filenames
Hashing and linking run as concurrent tokio tasks.
An already deduped store is skipped via `d_ino` from readdir without rehashing.
We measured ~2x faster than upstream on a warm store.

```
fast-nix-optimise [OPTIONS]

      --dry-run             Show what would be done
      --min-size BYTES      Skip files smaller than BYTES
  -j, --jobs N              Concurrency [default: num CPUs]
      --store-dir PATH      Nix store directory [default: /nix/store]
      --state-dir PATH      Nix state directory [default: /nix/var/nix]
```

Both tools take a shared `gc.lock` so they don't race each other or Nix.
While fast-nix-gc deletes, it serves the GC roots socket
(`state/gc-socket/socket` same as `nix-store --gc`).
A concurrent `nix build`s register temp roots without blocking on the lock.

## NixOS module

Replaces `nix.gc` and `nix.optimise`:

```nix
{
  inputs.fast-nix-gc.url = "github:Mic92/fast-nix-gc";

  outputs = { nixpkgs, fast-nix-gc, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        fast-nix-gc.nixosModules.default
        {
          services.fast-nix-gc = {
            enable = true;
            automatic = true;
            dates = "weekly";
            deleteOlderThan = "30d";
            ensureFree = "50G";
            keepRecent = "1d";
          };
          services.fast-nix-optimise = {
            enable = true;
            automatic = true;
            dates = "weekly";
          };
        }
      ];
    };
  };
}
```

`services.fast-nix-gc` options:

| Option | Default | Description |
|---|---|---|
| `enable` | `false` | Enable the systemd service |
| `automatic` | `false` | Run on a schedule via systemd timer |
| `dates` | `"03:15"` | When to run (`systemd.time(7)` calendar event) |
| `randomizedDelaySec` | `"0"` | Random delay before each run |
| `persistent` | `true` | Run on next boot if a scheduled run was missed |
| `deleteOlderThan` | `null` | Remove profile generations and stale gcroots/auto entries older than e.g. `"30d"` |
| `ensureFree` | `null` | Stop once this much disk is free, e.g. `"50G"` or `"20%"` of the store's filesystem |
| `keepRecent` | `null` | Pin paths registered within e.g. `"1d"` |
| `pruneDevshellRoots` | `true` | With `deleteOlderThan`: drop nix-direnv devshell roots not loaded within the window |
| `pruneAutoRoots` | `true` | With `deleteOlderThan`: drop other gcroots/auto links registered before the window |
| `noVacuum` | `false` | Skip the post-GC database VACUUM (see below) |
| `chunkSize` | `null` | Dead paths per delete transaction (default 65536) |
| `gcRootsDirs` | `[ ]` | Extra directories to scan for GC roots, e.g. `[ "/mnt/extra-roots" ]` |
| `package` | this flake's package | Override the binary |
| `extraArgs` | `[ ]` | Extra CLI arguments |

`services.fast-nix-optimise` options:

| Option | Default | Description |
|---|---|---|
| `enable` | `false` | Enable the systemd service |
| `automatic` | `false` | Run on a schedule via systemd timer |
| `dates` | `"04:15"` | When to run; ordered after fast-nix-gc.service when both run |
| `randomizedDelaySec` | `"0"` | Random delay before each run |
| `persistent` | `true` | Run on next boot if a scheduled run was missed |
| `minSize` | `null` | Skip files smaller than this many bytes |
| `jobs` | `null` | Concurrency, defaults to the CPU count |
| `package` | `services.fast-nix-gc.package` | Override the binary |
| `extraArgs` | `[ ]` | Extra CLI arguments |

Without flakes, import `nix/module.nix` directly.

### nix-darwin

`darwinModules.default` exposes the same `services.fast-nix-gc` and
`services.fast-nix-optimise` options, run as launchd daemons. Scheduling
uses launchd's `startCalendarInterval` (a list of
`launchd.plist(5)` StartCalendarInterval entries) instead of `dates`:

```nix
fast-nix-gc.darwinModules.default
{
  services.fast-nix-gc = {
    enable = true;
    automatic = true;
    startCalendarInterval = [ { Hour = 3; Minute = 15; } ];
    deleteOlderThan = "30d";
    ensureFree = "50G";
  };
}
```

### When to use `--no-vacuum` / `noVacuum`

After deleting dead paths, fast-nix-gc runs `VACUUM` when at least a
quarter of the database is free pages. In WAL mode this rewrites the
entire database through the write-ahead log. SQLite cannot truncate
the resulting database-sized `db.sqlite-wal` while any connection holds
a read snapshot. This is the reason Nix disabled GC vacuuming in commit
[`8299aaf`](https://github.com/NixOS/nix/commit/8299aaf07988a3ca7ecda3526b7e25a885550db5).

On builders that are never idle, enable `--no-vacuum`. The free pages
stay in `db.sqlite` and are reused for new registrations.
A later GC on a quiet system reclaims the space.

### Tuning `--chunk-size` / `chunkSize`

Dead paths are invalidated in batches, one SQLite transaction per batch,
with the write-ahead log truncated after each. The batch size trades disk
headroom against checkpoint overhead:

- **Smaller** keeps each transaction's `db.sqlite-wal` small. A single
  transaction over the whole dead set grows the WAL until commit.
  On a full disk that aborts with `SQLITE_FULL` and frees nothing.
  Chunking bounds the WAL and reclaims space incrementally.
- **Larger** means fewer transactions and fewer checkpoint `fsync`s, so
  deletion runs faster, at the cost of a larger transient WAL.

As a rule of thumb, deleting a path dirties scattered B-tree pages across
the references table and its indexes, costing very roughly 10 KiB of WAL
per dead path on a large, cold store, so a batch's WAL is about
`chunk-size × 10 KiB`. The default of 65536 keeps it near 640 MiB, safe on
all but a nearly full disk. With tens of GiB free, `--chunk-size 262144`
(~2.5 GiB WAL) cuts the checkpoint count ~4x.
Only higher when you have enough free disk space,
since the WAL grows linearly with the batch size.

The truncation after each batch can only reclaim WAL frames older than the
oldest live read snapshot. A long-running reader i.e. `nix-daemon` pins those frames,
so the WAL keeps growing across batches regardless of `--chunk-size`, potentially until the disk fills.
For a large GC on a tight disk stop `nix-daemon` (and any other store reader) first, or
keep the chunk size small enough that even an unreclaimed WAL fits the free
space.

## Building

    nix build

or `nix develop -c cargo build --release`. Without flakes:
`nix-build` (uses `default.nix`).

## Testing

    cargo test       # against a synthetic store in a tempdir
    cargo bench      # throughput across several synthetic store sizes

Two fuzzers back the behavioral claims below:

    # differential: random store graphs, roots, configs and corruption
    # vs nix-store --gc, deterministic per seed
    cargo run --release --bin fuzz-nix-diff -- --iterations 20

    # graph logic vs a reference model, no Nix (stable Rust)
    cargo fuzz run -s none --fuzz-dir fuzz gc_graph

## Behavior

Roots are gathered from `gcroots/`, `profiles/`, `temproots/`, and running
processes (`/proc` on Linux; `libproc` syscalls on macOS instead of
shelling out to `lsof`). Stale temp-root files and dangling auto-roots are
removed. `tmp-*` build dirs are skipped if a builder still holds the lock.

`keep-derivations` and `keep-outputs` are honored with the same
edge semantics as `nix-store --gc`: an alive output keeps its derivation
(`keep-derivations`), an alive derivation keeps its outputs
(`keep-outputs`). This includes content-addressed / dynamic derivations:
drv↔output mappings are read from `ValidPaths.deriver`,
`DerivationOutputs`, and the `BuildTraceV3` table (Nix ≥2.35).

The store is remounted read-write on NixOS where it's bind-mounted read-only.

### Database vacuum

SQLite never shrinks `db.sqlite` on its own, so after deletion the GC
runs `VACUUM` when at least 25% of the file is free pages, still under
the exclusive `gc.lock`. VACUUM is atomic: if it fails (e.g. out of
disk for the temp copy) the database stays valid and the next GC
retries. On a real-world 648 MB database that was 63% free pages,
vacuuming shrank it to 216 MB and made the cold-cache graph load 4x
faster (whole dry-run: 2.4x).

### Corrupted stores

Disk and database can disagree after crashes or tampering. The GC
repairs both directions: store entries without a database row are
deleted, and rows whose disk entry is missing are removed once the path
is garbage. Nix keeps a `.drv` row forever if its file is gone from
disk, and if that path is reachable from a gcroot, `nix-store --gc`
aborts entirely until the store is repaired by hand. fast-nix-gc
decides liveness from the database alone and handles both.

### Known limitation: keep-outputs with ca-derivations

On Nix without the `BuildTraceV3` table (older than 2.35), a derivation
depending on a floating content-addressed output has deferred output
paths: nothing in the database links it to its outputs until Nix
re-parses the `.drv` at GC time, which fast-nix-gc doesn't do. With
`keep-outputs = true` such a rooted derivation therefore does not keep
its outputs alive. This only loses cache, the outputs stay
rebuildable. Nix itself has no consistent behavior here: `--print-dead`,
the real GC and repeat runs all disagree, since the re-parsing happens
during the GC walk and is order-dependent. Revisit once upstream
defines stable semantics.

The GC takes the same `gc.lock` Nix does, so it won't race with
`nix-build` or another GC. If interrupted mid-run the DB stays
conservative: paths gone from the DB but still on disk are picked up as
unknown entries by the next GC.

## FAQ

**Could this have been implemented in upstream Nix?**

Yes, but C++ and concurrency is a scary combination,
which is why I went with this implementation first.

## Real-world timings

This is not a scientific benchmark, just `journalctl` data from a few of my
machines after switching the daily GC timer from `nix-gc.service` to
`fast-nix-gc.service`. Workloads differ between runs, sample sizes are
small, and hardware varies. Take it as a rough indication of scale, not a
controlled measurement.

| Host | Store profile | `nix-gc` wall clock | `fast-nix-gc` wall clock | Rough speedup |
| --- | --- | --- | --- | --- |
| Laptop (large store, daily dev churn) | ~100k+ paths | 1m25s – 19m, median ≈ 7m | 5s – 20s | ~25–60× |
| Build server (huge store, CI churn) | very large | 19m – 30m, avg ≈ 22m | 7s – 17s | ~80–180× |
| Small VPS (tiny store) | small | 4s – 23s | 15s | ~1–1.5× |

The speedup grows with store size: stock `nix-gc` pays a large fixed cost
walking the whole live closure even when there's almost nothing to delete
(near-idle runs still took minutes on the bigger machines), while
`fast-nix-gc` stays in the single-digit-seconds range.
On a tiny store the overhead is negligible either way and the two are roughly comparable.
