# Turso compatibility spike (T03, T03a)

This spike checks whether the local, embedded Turso engine, used through the `turso` Rust crate, fits BaukFS before production storage is built. T03 covered single-process behaviour. T03a checked Turso's experimental multiprocess WAL mode for the proposed layout: **one independent database per workspace copy, which several processes may open at the same time.**

The probe lives in `crates/baukfs-turso-spike`. It is a non-published binary crate with no library target, and nothing in `baukfs` or `baukfs-cli` depends on it. It uses its own probe tables (`probe_content`, `probe_head`, `probe_schema_version`), not the future BaukFS schema.

All observations in this document come from **x86_64-linux only**. No other platform was exercised.

## Recommendation

**One database per copy, opened in multiprocess WAL mode, is suitable with named constraints.** Same-copy concurrent commands work without a central daemon. Separate processes can open one copy's database together, read committed state while another process writes, and serialize writers through Turso's write lock.

Multiprocess WAL protects the database file. It does **not** serialize workspace-mutating operations; BaukFS still needs its own serialization for those.

Constraints production storage must respect:

1. **Every opener of a copy's database must use multiprocess mode, and nothing enforces it.** Through the `turso` crate in 0.8.3, a default-mode process can open *and write* a database that a multiprocess-mode process has open, and the reverse also works (see [Mixed open modes](#mixed-open-modes)). The two modes coordinate WAL readers and writers differently, so mixing them risks inconsistent reads or corruption. Route every open through a single helper that always sets `experimental_multiprocess_wal(true)`. External tools such as a stock `sqlite3` shell must not open a live copy.
2. **The busy timeout is a wait policy, not a deadline.** It bounds Turso's wait for one locking event. A competing writer with a 1000 ms timeout once succeeded only after 8184 ms under heavy test load (see [Timings](#timings)). Wrap storage operations in an independent operation deadline.
3. **Busy waiting spins a CPU core.** While waiting on the busy timeout, the engine wakes its own waker and the blocking I/O backend does nothing, so the waiting future is re-polled continuously. This was read from source and not measured. Keep busy timeouts short.
4. **Choose the busy timeout per connection, deliberately.** With no timeout a competing writer fails at once with `Busy`. With 1000 ms it waits and either succeeds or fails with `Busy` after about the full budget.
5. **Network filesystems are refused.** Turso rejects multiprocess WAL on NFS, SMB/CIFS, AFS, Coda, Ceph, GFS2, Lustre, NCP, OCFS2 and v9fs on Linux, and on nfs/smbfs/afpfs/webdav/cifs/acfs on macOS. Copies must live on a local filesystem.
6. **Constraints carried over from T03:**
   - Always commit or roll back explicitly. A dropped `Transaction` keeps the write lock until its connection is used again.
   - Database paths must be UTF-8.
   - Driver errors are a flat enum with string messages. Branch on the variant only.
   - Pin `turso = "=0.8.3"` with default features off.
7. **Durability against OS crashes and power loss is assumed, not tested.** See [Durability assumptions](#durability-assumptions).

## Dependency configuration

Unchanged from T03:

```toml
# crates/baukfs-turso-spike/Cargo.toml
[dependencies]
turso = { version = "=0.8.3", default-features = false }
```

| Item | Value |
| --- | --- |
| Crate | `turso` 0.8.3, crates.io checksum `f1cf9c519a3a971e845a42c870930557cb53c2368c5737d4c78f8fbfd0444e98` |
| Why this version | Newest non-prerelease on 2026-11-10. The 0.9.0 line is published only as `-pre.N`. |
| Enabled `turso` features | None |
| Toolchain | Rust 1.95.0, edition 2024. Turso crates declare no `rust-version`. |
| Async runtime | None. The probe drives the driver's futures with a ~40-line std-only `block_on` (`src/executor.rs`). |
| I/O backend | `IoBackend::Default`, which is `turso_core::UnixIO` (blocking syscalls) on Linux and macOS. io_uring code is compiled but not selected. |
| Remote access | None. No credentials, URLs, or network crates are in the graph. |

Sources inspected (the crates.io tarballs of the exact versions, not GitHub `main`):

- `turso` 0.8.3: <https://docs.rs/turso/0.8.3>, <https://crates.io/crates/turso/0.8.3>, repository <https://github.com/tursodatabase/turso> (tag `v0.8.3`)
- `turso_sdk_kit` 0.8.3 (`src/rsapi.rs`: database open, I/O backend selection, statement stepping)
- `turso_core` 0.8.3:
  - `io/unix.rs`: fsync, file locks, read-only opens
  - `statement.rs` and `busy.rs`: waker handling and busy waiting
  - `database.rs`: default `SyncMode::Full`, multiprocess WAL gating, mixed-mode guards

The SCE references used for the access pattern were `context/sce/shared-turso-db.md` and `cli/src/services/db/mod.rs` in `crocoder-dev/shared-context-engineering`. The probe adapts their approach (multiprocess WAL on every open, per-connection busy timeout, read-only readiness check, initialization under `BEGIN IMMEDIATE` with a recheck) without their `anyhow`-based error handling.

### Feature meanings (verified in 0.8.3 source)

| Feature | Default | Meaning | Decision |
| --- | --- | --- | --- |
| `mimalloc` | yes | Registers `mimalloc::MiMalloc` as `#[global_allocator]` | Off. A dependency should not choose the process allocator. |
| `fts` | yes | Full-text search via `tantivy` | Off. Not needed. |
| `sync` | no | **Turso Cloud synchronization**: `Builder::new_remote`, plus `hyper`, `hyper-rustls`, `rustls-native-certs`, and `tokio`. Not a synchronous Rust API. The Rust API is always `async`. | Off |
| `pure-rust-crypto`, `stacker`, `io_memory_yield`, `test_helper` | no | Encryption backend, deep-recursion stack growth, simulator and test hooks | Off |

Multiprocess WAL is not a Cargo feature. It is a runtime builder flag, `Builder::experimental_multiprocess_wal(true)`. It is compiled in on 64-bit Unix (`host_shared_wal` in `turso_core`'s build script), which covers all three supported targets.

## Open modes and connection settings

| Setting | How the probe applies it |
| --- | --- |
| Default mode | `Builder::new_local(path).experimental_multiprocess_wal(false).build()` |
| Multiprocess mode | `Builder::new_local(path).experimental_multiprocess_wal(true).build()` |
| Existing-only inspection | The same builder plus `.read_only(true)` |
| Busy timeout | `Connection::busy_timeout(Duration::from_millis(n))` on **every** connection the probe creates, immediately after `Database::connect`. `n = 0` leaves the busy handler unset. Tested with 0 and 1000 ms. |
| Write transactions | `BEGIN IMMEDIATE`, via `Connection::transaction_with_behavior` or `Transaction::new_unchecked` |

How each mode behaves on disk, from source and observation:

- **Default mode** takes an exclusive, non-blocking OS file lock for as long as the database is open.
- **Multiprocess mode** opens the database file with `NoLock` and coordinates through a memory-mapped `<db>-tshm` file. A multiprocess database directory held `probe.db`, `probe.db-wal` and `probe.db-tshm`.
- **Read-only opens** never create the database file and take no OS lock. If no `-tshm` file exists, they fall back to legacy read-only WAL access.

Every probe child must receive `--mode` and `--busy-timeout-ms`; there are no defaults. Each child prints `config mode=<mode> busy_timeout_ms=<ms>` as its first line, and the harness rejects any child whose line doesn't match the database's configured mode and timeout. Test databases carry their mode, so a test cannot open one in another mode by accident. The only mixed-mode opens are in the characterization test, which uses an explicitly named `deliberately_in_other_mode` helper.

## Probe commands

```
baukfs-turso-spike <COMMAND> --mode <default|multiprocess> --busy-timeout-ms <MS> <DATABASE>
```

| Command | Output after the `config` line |
| --- | --- |
| `roundtrip-write`, `roundtrip-verify` | `wrote 6 cases`, `verified 6 cases` |
| `transaction-commit`, `transaction-rollback` | `content=present head=present` or `content=absent head=absent` |
| `inspect` | Visibility of the interrupt records |
| `settings` | `journal_mode=wal synchronous=2` |
| `initialize` | `precheck <readiness>`, then `ok initialize outcome=<applied\|already-current\|current-after-lock> elapsed_ms=<n>` |
| `inspect-existing` | `missing`, `schema-not-current readiness=<r>`, or `current schema_cookie=<n> content=<…> head=<…>` |
| `session` | `ready`, then one reply per stdin command until `quit` |

Session commands: `begin`, `commit`, `rollback`, `put <key> <hex>`, `head <name> <key>`, `get <key> <name>`, `write-pair <key> <name> <hex>`, `init-hold`, `quit`. `write-pair` prints `started` before it tries to begin, so the parent knows when the attempt starts.

Failures inside a session are reported as:

```
err <step> <DriverErrorKind> elapsed_ms=<n> autocommit=<bool> <message>
```

The kind is the matched `turso::Error` variant, for example `Busy` or `BusySnapshot`. Elapsed times come from a monotonic clock (`Instant`) inside the child.

### Initialization pattern

`probe_schema_version` holds a single row with the probe schema version. This demonstrates the access pattern only; it is not a migration framework.

1. **Read-only precheck.** `readiness` queries `sqlite_schema` and the version row. A current database is opened with no write at all.
2. **Lock and recheck.** Otherwise `initialize_under_lock` begins `BEGIN IMMEDIATE` and checks readiness again while holding the write lock:
   - If another process finished first, it rolls back and reports `current-after-lock`.
   - Otherwise it creates the schema and version row inside that transaction and commits.
3. **Existing-only inspection.** `open_existing` opens read-only. A missing file returns `IoError(NotFound, "open")`, which the probe maps to `missing` without creating anything. Otherwise it runs the read-only readiness check and never issues DDL or `BEGIN IMMEDIATE`.

## Process harness

`tests/process/harness.rs` drives every child process:

- **One deadline per child.** A single deadline (1 minute for probes) covers readiness, every reply, stdout EOF, stderr EOF and process exit. Stdout closing does not lead to an unbounded wait for exit.
- **Output draining.** stdout and stderr are read on separate threads, so a child that floods stderr cannot block. After a kill, stdout is drained to EOF before the harness checks that nothing followed readiness.
- **Cleanup.** On any failure, and on drop, the child is killed and reaped.
- **Typed errors.** Failures are `HarnessError` values (`Deadline { stage }`, `ExitedBeforeLine`, `UnexpectedLine`, `OutputAfterReadiness`, `NotKilled`, `Failed`, and I/O variants). Each carries the child's command line and complete stderr.
- **Waiting.** Every wait is event-driven: readiness lines, replies, EOF. Process exit is polled every 2 ms against the deadline, because std has no event for it. No sleep decides an outcome.

The one deliberate sleep is in the busy-release scenario. It is the scenario's 250 ms hold time. The checks around it hold however long the sleep actually takes: the competitor must not have replied while the lock is held, and it must succeed after release.

Harness regression tests (`tests/process/harness_regression.rs`) use `sh` built-ins as misbehaving children:

| Test | Child behaviour | Expected harness result |
| --- | --- | --- |
| `stdout_eof_without_exit_hits_the_exit_deadline` | Closes stdout, keeps running | `Deadline { stage: Exit }` within budget plus cleanup |
| `missing_readiness_hits_the_line_deadline` | Never prints | `Deadline { stage: Line }`, child reaped |
| `exit_before_readiness_reports_status_and_stderr` | `echo boom >&2; exit 3` | `ExitedBeforeLine` with code 3 and stderr `boom` |
| `stderr_flood_before_readiness_does_not_block` | 266,240 bytes of stderr, then `ready` | Readiness received; all stderr collected |
| `output_after_readiness_is_drained_and_reported` | Prints `ready` and `late` in one write | `OutputAfterReadiness(["late"])` |
| `dropped_probe_kills_and_reaps_its_child` | Waits on stdin | Process gone after the guard drops |

## Reproducing

Run inside `nix develop`. The host shell has no Rust toolchain.

```sh
nix develop -c cargo test -p baukfs-turso-spike                                 # 13 unit + 30 process tests
nix develop -c cargo test -p baukfs-turso-spike --test process -- --nocapture   # prints timing lines
nix develop -c cargo test -p baukfs-turso-spike --test process multiprocess::   # one module
```

To run the commands by hand against a scratch database:

```sh
db=$(mktemp -d)/probe.db
run() { nix develop -c cargo run -q -p baukfs-turso-spike -- "$@"; }
run initialize       --mode multiprocess --busy-timeout-ms 1000 "$db"   # precheck uninitialized / outcome=applied
run inspect-existing --mode multiprocess --busy-timeout-ms 0 "$db"      # current schema_cookie=… content=absent head=absent
run session          --mode multiprocess --busy-timeout-ms 0 "$db"      # then type: begin, put k 00ff, head h k, commit, get k h, quit
```

## Results on x86_64-linux

Host: Linux 6.18.37, ext4, rustc 1.95.0 (Nix dev shell).

### Default mode (controls)

Each scenario uses its own database, opened only in default mode.

| Scenario | Result |
| --- | --- |
| Binary round trip, fresh-process reopen | Exact match for empty, NUL, invalid UTF-8, all 256 byte values, and 1 MiB |
| Commit / rollback, checked from a fresh process | Both records / neither |
| `journal_mode`, `synchronous` | `wal`, `2` (FULL) |
| Second process opens while one holds the database | Exit 1, `Error("Locking error: Failed locking file '<path>'. File is locked by another process")` |
| SIGKILL after both writes, before commit | Fresh process: `content=absent head=absent` |
| SIGKILL after commit acknowledgement | Fresh process: `content=present head=present` |

### Multiprocess mode

Every participating process opens with `experimental_multiprocess_wal(true)`.

| Scenario | Result |
| --- | --- |
| `journal_mode`, `synchronous` | `wal`, `2` (FULL) |
| Two processes keep the database open | Both read; a write from one is visible to the other's next query |
| B reads while A holds an uncommitted content + head transaction | B sees the prior committed records and neither new record; after A commits, B's next query on the same connection sees both |
| Competing writer, busy timeout 0 | `err begin Busy … autocommit=true Begin transaction: database is locked`; no transaction left open |
| Competing writer, busy timeout 1000 ms, A releases after 250 ms | B had not replied while A held the lock; then `ok write-pair`; both pairs visible |
| Competing writer, busy timeout 1000 ms, A keeps holding | `err begin Busy … autocommit=true` after ≥ 1000 ms; B's records absent; A's uncommitted records invisible to B |
| After contention clears | B's retried write and a new transaction from A both commit |
| SIGKILL of the writer after both writes, before commit | A surviving reader sees neither record, then writes successfully (the killed writer's lock is released); fresh process and `inspect-existing` see neither record |
| SIGKILL after commit acknowledgement | The surviving reader sees both records and writes; a fresh process sees everything committed |

### Initialization and inspection (multiprocess mode)

| Scenario | Result |
| --- | --- |
| A applies the schema under `BEGIN IMMEDIATE` and holds the transaction; B runs `initialize` | B's precheck reads `uninitialized`; after A commits, B reports `current-after-lock` and applies nothing; a later `initialize` reports `already-current` |
| 4 `initialize` processes started together (busy timeout 1000 ms) | Exactly one `applied`, three `current-after-lock`, in every observed run |
| `inspect-existing` on a current database | Database and WAL files byte-identical before and after, no new files, schema cookie unchanged (observed: 3) |
| `inspect-existing` while another process holds a write transaction (busy timeout 0) | Completes (~6 ms process lifetime); sees only committed state; the writer is unaffected |
| `inspect-existing` on a missing path | `missing`; the directory stays empty (no database, `-wal`, or `-tshm` file) |
| `inspect-existing` on an existing, uninitialized database | `schema-not-current readiness=uninitialized` |

### Independent copy databases (multiprocess mode)

Copies A and B live in separate directories.

| Scenario | Result |
| --- | --- |
| Writers hold open transactions on A and B at the same time | Both `BEGIN IMMEDIATE` succeed promptly with no busy timeout; both commit |
| Identical key, different contents | A returns `aa00ff`, B returns `bb01fe` for `shared-content` |
| A holds a write; B runs `write-pair` with busy timeout 0 | Succeeds (20–67 ms per run, under parallel test load) |
| A's writer rolls back, then another A writer is killed mid-transaction | B's committed record is unchanged; A shows neither record |

### Mixed open modes

`mixed_open_modes_are_not_rejected_through_the_turso_crate` records the observed behaviour:

| Holder (keeps the database open) | Second process | Result |
| --- | --- | --- |
| Multiprocess | Default-mode `inspect` | Succeeds |
| Multiprocess | Default-mode `transaction-commit` | Succeeds and commits |
| Default | Multiprocess `inspect` | Succeeds |
| Default | Multiprocess `transaction-commit` | Succeeds and commits |

`turso_core` has guards that reject both directions ("already open with/without experimental multiprocess WAL in another process"). They run inside `Database::resolve_default_storage`, which is used only when the caller does not supply storage. `turso_sdk_kit`, behind the `turso` crate, opens the file itself and passes it in as storage, so the guards never run. This is the basis for constraint 1. If a later Turso release enforces the guards, this test will fail and the constraint should be revisited.

### Typed errors

| Case | Mode | `turso::Error` |
| --- | --- | --- |
| Duplicate primary key | default | `Constraint("UNIQUE constraint failed: probe_content.probe_key")` |
| Missing parent directory | default | `IoError(NotFound, "open")` |
| Path is a directory | default | `IoError(IsADirectory, "open")` |
| Not a database | default | `NotAdb("file is not a database")` |
| Read-only open of a missing file | both | `IoError(NotFound, "open")`, mapped by the probe to `ExistingOpen::Missing` |
| Second default-mode process | default | `Error("Locking error: … File is locked by another process")` |
| Competing `BEGIN IMMEDIATE` | both | `Busy("database is locked")`, reported by the probe as `Begin transaction: database is locked` |

`BusySnapshot` (a read snapshot that cannot upgrade to a write) was never observed. The probe always uses `BEGIN IMMEDIATE`, so it takes the write lock before reading. Tests accept either variant as the busy family.

### Timings

Elapsed times are measured with `Instant` in the child and, separately, in the parent around the request. They come from `--nocapture` runs while the rest of the suite ran in parallel.

| Measurement | Observed |
| --- | --- |
| Competing writer, busy timeout 0 | `Busy` after 0.7–0.8 ms |
| Competing writer, busy timeout 1000 ms, lock kept | `Busy` after 1001.3–1001.5 ms (child), 1001.4–1001.6 ms (parent) |
| Competing writer, busy timeout 1000 ms, lock released after 250 ms | Committed after 344–486 ms in all but one of about 180 runs. The holder's own commit took 8–111 ms. |
| Same scenario, outlier | **Once in 120 full-suite runs: committed after 8184 ms**, not reproduced in 60 isolated runs or in 80 later full-suite runs that also timed the holder's commit; cause unknown |
| Reader query during an uncommitted write | 1.2 ms round trip |
| Write by a survivor right after the writer was killed mid-transaction | 23–60 ms |

After the lock is released, a waiting writer commits a further ~90–100 ms later. Turso's busy backoff sleeps in growing phases (1, 2, 5, 10 … ms, then 100 ms per phase after the first 228 ms), so a waiter can notice a release up to one phase late.

### Repeat runs

The process suite was run 120 times in full: 40 runs, then 80 runs with commit timing added. Two runs failed:

1. **The 8184 ms busy-release outlier above.** The test asserts the competitor finishes inside its busy timeout, so a repeat of the outlier will fail it again until that assertion is revisited.
2. **A race in the harness fixture.** The child printed `ready` and `late` with two separate `echo`s, so a kill could land between them. It now emits both lines in one `printf`. That test passed after the fix; the full repeat loop has not been re-run since.

## Process termination versus durability

**Process termination (tested).** In both modes, SIGKILL of a writer after both inserts and before `COMMIT` exposed neither record, and SIGKILL after `commit` returned `Ok` preserved both. No partial visibility was ever observed. In multiprocess mode, a peer that stayed alive across the kill kept reading, and then took the write lock and committed.

### Durability assumptions

The termination tests prove atomicity and commit visibility only when a process dies while the OS keeps running. They do not cover kernel panics, power loss, or storage that acknowledges writes before they are durable. Durability across those events assumes:

- With `synchronous=FULL`, the engine fsyncs the WAL before acknowledging a commit, as in SQLite's WAL design. This was read from source, not observed.
- The filesystem and device honour `fsync`. On macOS, plain `fsync` does not flush the drive cache, so `PRAGMA fullfsync = ON` should be evaluated there.
- Testing these assumptions needs fault injection, such as dm-flakey or a VM power cut, which this spike does not do.

## Dependency review

Unchanged from T03. Direct dependency: `turso = "=0.8.3"` with no features. It resolves 231 normal-dependency packages (260 including build-time).

| Concern | Finding |
| --- | --- |
| `anyhow` | Present in `Cargo.lock`, pulled in only by `prost-derive` (a proc-macro, run at build time) under `turso_sync_engine`. No first-party manifest declares it. |
| `async_trait`, `async_recursion` | Not in the graph. |
| `dyn` and `Box<dyn …>` in driver internals | Extensive. `turso_core` has ~780 `dyn` occurrences, including `Arc<dyn IO>`, `Arc<dyn File>`, and the busy handler's `Box<dyn Fn(i32) -> i32>`. **The dependency graph is not statically dispatched.** |
| Erased types in the public API | `turso::Error::ToSqlConversionFailure(Box<dyn Error + Send + Sync>)`, and `Builder::with_io_impl(Arc<dyn turso_core::IO>)`. The probe names neither; its `DriverErrorKind` classification matches the variant without opening its payload. |
| Error chains | Variants carry `String`s. The probe never calls `source()`. |
| Forbidden first-party adapters needed? | **No.** The T03a additions use concrete driver types and generic `async fn`. They add no `dyn`, no authored `Box`, no boxed futures and no `source()` traversal. `architecture/check.sh` and Clippy pass on the new code. |
| Async runtime | Not required. See constraint 3 for the busy-wait CPU cost. |
| `unsafe` | Present throughout the engine, including `mmap` of the `-tshm` file in multiprocess mode. Needs its own review if that matters for BaukFS. |

### Native build requirements

- **A C compiler** for `simsimd` (via `turso_core`'s unconditional `simd` feature). The Nix stdenv provides one.
- `bindgen` and `clang-sys` are compiled as build dependencies, but their build scripts exit early off Windows, so **libclang is not required**.
- `turso_core`'s build script runs `git rev-parse` to embed a source id. It skips git when `SOURCE_DATE_EPOCH` is set (as in Nix).

## Validation status (T03a)

| Check | Status |
| --- | --- |
| `cargo test -p baukfs-turso-spike` | 13 unit and 30 process tests passed after the latest changes except the final fixture fix, which was verified by running that test alone |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo fmt --all` | Clean at the last full run |
| Full workspace tests, architecture fixtures and checker, Nix checks | **Not yet re-run for T03a.** The new `tests/process/` files must be `git add`ed before the flake can see them. |

## Remaining platform verification

- **aarch64-linux:** same code paths as x86_64-linux (`UnixIO`, OFD locks, `statfs` magic filter). Run `nix build -L .#checks.aarch64-linux.test` on a native or emulated builder.
- **aarch64-darwin:** not tested. Differences that matter:
  - It uses process-scoped `fcntl` locks instead of Linux OFD locks. Re-check default-mode locking, the in-process second-handle probe, and the dead-writer lock release.
  - Multiprocess coordination still uses an `mmap`ed `-tshm` file, and the filesystem filter is a name list (`f_fstypename`). Re-run every multiprocess, initialization and independent-copy test.
  - `fsync` does not imply `F_FULLFSYNC`. Decide whether to set `PRAGMA fullfsync`.
  - `simsimd` builds with Apple clang.
  - Run `nix build -L .#checks.aarch64-darwin.test`.
- **All platforms:**
  - Power-loss and OS-crash durability.
  - The cause of the busy-release outlier.
  - Behaviour under sustained multi-writer load beyond these short scenarios.

## Build integration

- `crates/baukfs-turso-spike` is a workspace member (`publish = false`, workspace lints). The Nix `fmt`, `clippy`, `test`, and `architecture` checks cover it automatically, and each still runs on its own. The process tests need `/bin/sh`, which the Nix sandbox provides.
- The `build` check and the default package build only `baukfs-cli` (`cargoExtraArgs = "--locked -p baukfs-cli"`). The probe binary does not ship, and `baukfs-cli --help` is unchanged.
