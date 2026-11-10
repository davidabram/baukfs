# Turso compatibility spike (T03)

This spike checks whether the local, embedded Turso engine, used through the `turso` Rust crate, fits BaukFS before production storage is built. The probe lives in `crates/baukfs-turso-spike`. It is a non-published binary crate with no library target, and nothing in `baukfs` or `baukfs-cli` depends on it. It uses its own probe tables (`probe_content` and `probe_head`), not the future BaukFS schema.

## Recommendation

**Suitable, with named constraints.** Every essential probe passed on `x86_64-linux`. The driver needs no forbidden first-party architecture. Production storage must respect these constraints:

1. **Single-process ownership.** Opening a database takes an exclusive OS file lock. While one process has the database open, any other process fails to open it, whether or not a transaction is active. The failure is the unstructured `turso::Error::Error("Locking error: … File is locked by another process")`. BaukFS must route all access through one owning process. Turso has an opt-in `experimental_multiprocess_wal` mode, but it is experimental and this spike did not evaluate it.
2. **Always finish transactions explicitly.** A dropped `Transaction` is not rolled back when it is dropped. It stays open, and keeps the write lock, until the owning `Connection` is next used. Call `commit` or `rollback` on every path.
3. **One writer at a time, no busy timeout by default.** A second `BEGIN IMMEDIATE` on the same database fails at once with `turso::Error::Busy("database is locked")`. Serialize writers in BaukFS, or set `Connection::busy_timeout` deliberately.
4. **Database paths must be UTF-8.** `Builder::new_local` accepts only `&str`. The probe rejects non-UTF-8 paths with its own typed variant before calling the driver.
5. **Error detail is variant plus message.** Driver errors are a flat enum with `String` payloads. Constraint violations carry no structured constraint kind, column, or extended code. Code that branches on errors should match on the variant (`Constraint`, `Busy`, `NotAdb`, `IoError(kind, op)`) and treat the message as diagnostic text only.
6. **Pin the version exactly and default features off.** Defaults install `mimalloc` as the process's global allocator. The README on crates.io is stale: it still shows version 0.4.3 and tokio examples.
7. **Durability against OS crashes and power loss is assumed, not tested.** See [Durability assumptions](#durability-assumptions).

## Dependency configuration

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
| Toolchain | Rust 1.95.0, edition 2024. The workspace builds without changes. Turso crates declare no `rust-version`. |
| Async runtime | None. The probe drives the driver's futures with a ~40-line std-only `block_on` (`src/executor.rs`). |
| I/O backend | `IoBackend::Default`, which is `turso_core::UnixIO` (blocking syscalls) on Linux and macOS. io_uring code is compiled but not selected. |
| Remote access | None. No credentials, URLs, or network crates are in the graph. |

Sources inspected (the crates.io tarballs of the exact versions, not GitHub `main`):

- `turso` 0.8.3: <https://docs.rs/turso/0.8.3>, <https://crates.io/crates/turso/0.8.3>, repository <https://github.com/tursodatabase/turso> (tag `v0.8.3`)
- `turso_sdk_kit` 0.8.3 (`src/rsapi.rs`: database open, I/O backend selection, statement stepping)
- `turso_core` 0.8.3 (`io/unix.rs`: fsync and file locks; `statement.rs`: waker handling; `database.rs`: default `SyncMode::Full`)

### Feature meanings (verified in 0.8.3 source)

| Feature | Default | Meaning | Decision |
| --- | --- | --- | --- |
| `mimalloc` | yes | Registers `mimalloc::MiMalloc` as `#[global_allocator]` | Off. A dependency should not choose the process allocator. |
| `fts` | yes | Full-text search via `tantivy` | Off. Not needed. |
| `sync` | no | **Turso Cloud synchronization**: `Builder::new_remote`, plus `hyper`, `hyper-rustls`, `rustls-native-certs`, and `tokio`. Not a synchronous Rust API. The Rust API is always `async`. | Off |
| `pure-rust-crypto`, `stacker`, `io_memory_yield`, `test_helper` | no | Encryption backend, deep-recursion stack growth, simulator and test hooks | Off |

Even with `default-features = false`, `turso` 0.8.3 depends unconditionally on `turso_sync_sdk_kit` (and through it `turso_sync_engine`). The `turso` crate references that dependency only from its `sync` module, which is gated by the `sync` feature. The sync code is compiled but unreachable from the local API, and none of the HTTP, TLS, or tokio crates are pulled in. `turso_sdk_kit` enables these `turso_core` features unconditionally: `io_uring`, `experimental_win_iocp`, `conn_raw_api`, `encryption`, and `pure-rust-crypto`.

## Reproducing the probes

Run everything inside `nix develop`. The host shell has no Rust toolchain or `rg`.

```sh
nix develop -c cargo test -p baukfs-turso-spike                 # all 16 probes
nix develop -c cargo test -p baukfs-turso-spike -- --nocapture  # with test output
nix build -L .#checks.$(nix config show system).test           # same probes in the Nix sandbox
```

To run the probe commands by hand against a scratch database:

```sh
db=$(mktemp -d)/probe.db
nix develop -c cargo run -q -p baukfs-turso-spike -- roundtrip-write "$db"       # wrote 6 cases
nix develop -c cargo run -q -p baukfs-turso-spike -- roundtrip-verify "$db"      # verified 6 cases (fresh process)
nix develop -c cargo run -q -p baukfs-turso-spike -- transaction-commit "$db"    # content=present head=present
nix develop -c cargo run -q -p baukfs-turso-spike -- inspect "$db"               # content=present head=present
nix develop -c cargo run -q -p baukfs-turso-spike -- settings "$db"              # journal_mode=wal synchronous=2
```

`hold-uncommitted` and `hold-committed` print `ready: uncommitted` or `ready: committed`, then block on stdin until terminated. `tests/process.rs` drives them.

| Probe | Test | Where |
| --- | --- | --- |
| File-backed open, schema, parameterized write and read, exact binary round trip | `probe_tests::binary_contents_round_trip_exactly` | in-process |
| Empty blob stays `Value::Blob([])`, not NULL | `probe_tests::empty_blob_is_not_null` | in-process |
| Close all handles, reopen from a fresh process | `binary_contents_survive_a_fresh_process` | 2 child processes |
| Commit preserves content and head | `probe_tests::commit_preserves_both_records`, `committed_transaction_is_visible_to_a_fresh_process` | both |
| Explicit rollback preserves neither | `probe_tests::rollback_preserves_neither_record`, `rolled_back_transaction_is_invisible_to_a_fresh_process` | both |
| Termination after both writes, before commit | `termination_before_commit_exposes_neither_record` | child process, SIGKILL |
| Termination after commit acknowledgement | `termination_after_commit_exposes_both_records` | child process, SIGKILL |
| Duplicate key | `probe_tests::duplicate_key_reports_constraint` | in-process |
| Open failures | `probe_tests::open_failures_keep_driver_variants` | in-process |
| Isolation across connections | `probe_tests::separate_connection_does_not_see_uncommitted_writes` | in-process |
| Second `Database` handle in one process | `probe_tests::second_database_handle_in_same_process_shares_committed_state` | in-process |
| Dropped transaction behavior | `probe_tests::dropped_transaction_stays_open_until_next_use` | in-process |
| Default durability settings | `probe_tests::default_durability_settings`, `durability_settings_in_a_fresh_process` | both |
| Second process opening while another holds the database | `assert_second_process_cannot_open`, called in both termination tests | child process |

Binary cases: empty, a single NUL, embedded NULs, invalid UTF-8 (`ff fe c3 28 80`), all 256 byte values, and 1 MiB of pseudo-random bytes (overflow pages). For each case the probe checks the bytes, `typeof(bytes) = 'blob'`, and `length(bytes)`.

### Process interruption method

The writer runs as a child process. It reports readiness on stdout only after the relevant driver call has returned: after both inserts succeed for `hold-uncommitted`, and after `commit` returns `Ok` for `hold-committed`. It then blocks on a piped stdin. The parent:

1. Waits for the exact readiness line through a channel with a 1-minute bound. On a timeout, an unexpected line, or early exit, it kills the child and fails with the child's exit status and stderr.
2. Checks the child is still running, then sends SIGKILL (`Child::kill`). It reaps the child and asserts `signal() == Some(9)` and that nothing was printed after readiness.
3. Inspects the database from a new child process and asserts `content` and `head` separately. A partial result (`present`/`absent` mixed) fails the test.

`ChildGuard` kills and reaps any child still running when a test panics. Every wait is event-driven (readiness line, stdout EOF, `wait`). No sleep decides an outcome.

## Observed behavior (x86_64-linux)

### Transactions

- The probe begins transactions with `Connection::transaction_with_behavior(TransactionBehavior::Immediate)`, which takes `&mut Connection`. Nested transactions on one connection are therefore prevented at compile time.
- `Transaction::commit` and `Transaction::rollback` consume the transaction and run `COMMIT` or `ROLLBACK`.
- Inside the transaction, both records are visible. After commit, both are visible to the same connection, to other connections, and after reopening in a fresh process. After rollback, neither is visible anywhere.
- **Dropping a transaction rolls back lazily.** `Drop` cannot run async code, so it records `DropBehavior::Rollback` in the connection. Afterwards `is_autocommit()` is `false` and another connection's `BEGIN IMMEDIATE` fails with `Busy`. The pending `ROLLBACK` runs on the owning connection's next `execute` or `query`. After that, the records are absent and other writers proceed.

### Process termination

| Scenario | Fresh-process inspection |
| --- | --- |
| SIGKILL after both inserts, before `COMMIT` | `content=absent head=absent` |
| SIGKILL after `commit` returned `Ok` | `content=present head=present` |

No partial visibility was observed. The OS releases the file lock when the process dies, so the next process opens normally and recovers from the WAL. Both tests passed 25 consecutive runs on the host and also passed in the Nix sandbox (release profile).

### Errors

| Case | Driver error (`turso::Error`) |
| --- | --- |
| Duplicate `probe_content.probe_key` | `Constraint("UNIQUE constraint failed: probe_content.probe_key")` |
| Duplicate `probe_head.head_name` | `Constraint("UNIQUE constraint failed: probe_head.head_name")` |
| Parent directory missing | `IoError(ErrorKind::NotFound, "open")` |
| Path is a directory | `IoError(ErrorKind::IsADirectory, "open")` |
| Existing file is not a database | `NotAdb("file is not a database")`, at `Builder::build`, before any query |
| Second process opens a database that is held open | `Error("Locking error: Failed locking file '<path>'. File is locked by another process")` |
| Second `BEGIN IMMEDIATE` while a write transaction is open | `Busy("database is locked")` |

The probe keeps the concrete `turso::Error` inside typed variants (`ProbeError::Open { path, error }`, `Write { table, error }`, `Transaction { step, error }`, and so on). Tests match on those variants and never on formatted strings. `turso::Error` implements `std::error::Error` via `thiserror`. The probe never calls `source()`.

### Connections

- `Database` is a cheap `Clone` around an `Arc`. `Database::connect` returns independent `Connection`s, and statements and rows hold clones of their connection. Dropping every `Database`, `Connection`, `Statement`, and `Rows` closes the database. There is no explicit `close`, and a fresh process opened cleanly after every probe.
- A reader on another connection sees only committed data while a writer's transaction is open. Its reads do not block or fail.
- A second `Builder::new_local(path).build()` for the same path in the same process succeeds and shares committed state with the first handle.

### Durability settings

Verified at runtime on a new database: `PRAGMA journal_mode` = `wal` and `PRAGMA synchronous` = `2` (FULL). `turso_core` defaults connections to `SyncMode::Full`. Its `UnixIO::sync` calls `fsync(2)` on Linux. On Apple platforms it calls `fsync(2)` unless `PRAGMA fullfsync` is on, in which case it uses `fcntl(F_FULLFSYNC)`. The spike changes no settings.

### Durability assumptions

The termination tests prove atomicity and commit visibility only when a process dies while the OS keeps running. They do not cover kernel panics, power loss, or storage that acknowledges writes before they are durable. Durability across those events assumes:

- The engine, as in SQLite's WAL design, fsyncs the WAL before acknowledging a commit when `synchronous=FULL`. The spike read this from the source but did not observe it.
- The filesystem and device honor `fsync`. On macOS, plain `fsync` does not flush the drive cache, so BaukFS should evaluate `PRAGMA fullfsync = ON` there.
- Testing these assumptions requires fault injection (for example dm-flakey or a VM power cut), which T03 does not do.

## Dependency review

Direct dependency: `turso = "=0.8.3"` with no features. It resolves 231 normal-dependency packages (260 including build-time). Notable members:

| Concern | Finding |
| --- | --- |
| `anyhow` | Present in `Cargo.lock`, pulled in only by `prost-derive` (a proc-macro, run at build time) under `turso_sync_engine`. `turso/src/sync.rs` also uses it, but that module is not compiled without the `sync` feature. No first-party manifest declares it, and the architecture checker still passes. |
| `async_trait`, `async_recursion` | Not in the graph. |
| `dyn` and `Box<dyn …>` in driver internals | Extensive: `turso_core` has ~780 `dyn` occurrences, including `Arc<dyn IO>` and `Arc<dyn File>` for the I/O layer and `Box<dyn …>` callbacks. The `turso` crate itself uses `Arc<dyn Fn(Waker)>` for sync I/O hooks and a boxed statement inside `Statement`. **The dependency graph is not statically dispatched.** |
| Erased types in the public API | `turso::Error::ToSqlConversionFailure(Box<dyn Error + Send + Sync>)`. Parameter binding goes through a blanket `IntoValue` impl whose conversion error is erased into that variant. `Builder::with_io_impl` takes `Arc<dyn turso_core::IO>`. The probe never names either one. It matches `turso::Error` variants it expects and leaves this one unmatched. |
| Error chains | `turso::Error` variants carry `String`s, not nested errors, so there is no chain to traverse. The `Batch*` variants nest `Box<Error>` and the probe does not use them. |
| `thiserror` | Used by `turso` for `Display` and `Error` impls only. It is compatible with our policy because we do not call `source()`. |
| Forbidden first-party adapters needed? | **No.** The probe uses only concrete driver types (`Builder`, `Database`, `Connection`, `Transaction<'_>`, `Rows`, `Row`, `Value`, `turso::Error`), generic `async fn`, and a std `Waker` built from `Arc<impl Wake>`. No `dyn`, authored `Box`, boxed futures, `async_trait`, or `source()` traversal. `architecture/check.sh` and Clippy pass on the new crate unchanged. |
| Async runtime | Not required. The driver runs blocking `UnixIO` inside `poll` and wakes the registered waker on I/O completion. `block_on` re-polls after 5 ms if a wake never arrives; this bounds a missed wake to a delay and does not affect correctness. |
| `tracing`, `tracing-subscriber`, `tracing-appender`, `env_logger` | Unconditional dependencies of `turso` and `turso_sdk_kit`. The probe installs no subscriber. |
| `unsafe` | Present throughout the engine (FFI to libc, page buffers). Expected for a storage engine. Needs its own review if that matters for BaukFS. |

### Native build requirements

- **A C compiler** for `simsimd` (via the `cc` crate in `turso_core`'s unconditional `simd` feature) and `libc`. The crane/Nix stdenv provides one. No other system libraries are linked.
- `bindgen` and `clang-sys` are build dependencies of `turso_sdk_kit` and `turso_sync_sdk_kit`. Their build scripts return early on non-Windows targets, so **libclang is not required** on our targets. The Nix sandbox build, whose inputs include no libclang, succeeds.
- `turso_core`'s build script runs `git rev-parse` to embed a source id. It skips git when `SOURCE_DATE_EPOCH` is set (as in Nix) and ignores a missing git.
- Cold build cost: about 30 s of dependency compilation on the test host (debug profile).

## Results for the tested host

| Item | Value |
| --- | --- |
| System | `x86_64-linux`, Linux 6.18.37, ext4 (`target/tmp` and `/tmp`) |
| Toolchain | rustc 1.95.0, cargo 1.95.0 (Nix dev shell) |
| `cargo test -p baukfs-turso-spike` | 16 passed (10 in-process, 6 process). 25 consecutive repeat runs, 0 failures. |
| `cargo test --workspace` | All passed, including the 13 T02 domain tests |
| `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings` | Clean |
| `architecture/self-test.sh`, `architecture/check.sh .` | 192/192 fixtures passed, checker ok (10 Rust files, 4 manifests) |
| Nix checks `architecture`, `fmt`, `clippy`, `test`, `build` | All passed. `test` ran all 16 probes in the sandbox under the release profile. |

## Remaining platform verification

- **aarch64-linux:** the same code paths as x86_64-linux (`UnixIO`, OFD locks). Run `nix build -L .#checks.aarch64-linux.test` on a native or emulated builder.
- **aarch64-darwin:** not tested. It differs in ways that matter:
  - It uses process-scoped `fcntl` locks instead of Linux OFD locks. Re-check the single-process lock error and the in-process second-handle probe.
  - `fsync` does not imply `F_FULLFSYNC`. Decide whether to set `PRAGMA fullfsync`.
  - `simsimd` builds with Apple clang.
  - Run `nix build -L .#checks.aarch64-darwin.test`.
- **All platforms:** power-loss and OS-crash durability (see above) and `experimental_multiprocess_wal`, if multi-process access is ever needed.

## Build integration

- `crates/baukfs-turso-spike` is a workspace member (`publish = false`, workspace lints). The Nix `fmt`, `clippy`, `test`, and `architecture` checks cover it automatically, and each still runs on its own.
- The `build` check and the default package build only `baukfs-cli` (`cargoExtraArgs = "--locked -p baukfs-cli"`). The probe binary does not ship in `nix build` or `nix run`, and `baukfs-cli --help` output is unchanged.
- `Cargo.lock` now pins the Turso graph.
