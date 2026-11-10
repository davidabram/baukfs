# Workspace storage (T05)

`baukfs::storage` opens, identifies, and initializes the local Turso database of one workspace copy. Each copy has its own independent database. This step covers identity, schema readiness, initialization, and opening. File contents, manifests, and revisions come later (see [Deferred responsibilities](#deferred-responsibilities)).

The domain modules (`WorkspaceId`, `Workspace`, …) have no database code. Storage depends on them; they do not depend on storage.

## Public API

| Item | Purpose |
| --- | --- |
| `WorkspaceStore::create(path, &WorkspaceId, BusyTimeout)` | Explicit creation. Initializes an absent or empty database for the given workspace, or opens it if it is already initialized for that workspace. Returns `(WorkspaceStore, CreateOutcome)`. |
| `WorkspaceStore::open_existing(path, &WorkspaceId, BusyTimeout)` | Opens an existing database for normal use. It must be current and belong to the given workspace. |
| `storage::inspect(path, BusyTimeout)` | Read-only inspection. Returns a `DatabaseState`. |
| `WorkspaceStore::{workspace_id, busy_timeout, read_state}` | The stored identity, the connection's wait policy, and a re-read of the state through the store's own connection. |
| `CreateOutcome` | `Created`, `AlreadyInitialized` (found by the read-only check, no write lock taken), or `InitializedConcurrently` (another initializer for the same workspace committed while this call waited for the lock). |
| `DatabaseState` | `Missing`, `Uninitialized`, `Current(WorkspaceId)`, `UnsupportedSchemaVersion { found }`, or `Unrecognized(Unrecognized)`. |
| `Unrecognized` | `NotADatabase`, `ForeignSchema` (schema objects present, no BaukFS metadata table), `IncompleteSchema` (current version but required objects missing), `MalformedMetadata` (not exactly one well-formed metadata row). |
| `BusyTimeout` | Per-connection lock-wait policy. `BusyTimeout::IMMEDIATE` or `BusyTimeout::new(Duration)`. |
| `StorageError` | `NonUtf8Path`, `Driver(DriverError)`, `Missing`, `Uninitialized`, `UnsupportedSchemaVersion`, `Unrecognized`, `WorkspaceMismatch { requested, stored }`, `RollbackFailed { abandoned, rollback }`. |
| `DriverError` | The failing `DriverStep` and a `DriverErrorKind`, classified from the driver's error variant. Its fields are private. |
| `CURRENT_SCHEMA_VERSION` | `1`. |

All operations are `async fn`s that drive the driver's own futures. The library does not own a runtime, spawn threads, or use blocking wrappers. Callers supply the executor. The tests use a small spinning `block_on`.

`WorkspaceStore` holds one private `turso::Connection`. There are no backend traits, pools, or erased types.

## Database identity and bootstrap schema

Version 1 consists of one table and three triggers, created in a single transaction:

```sql
CREATE TABLE baukfs_metadata (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    schema_version INTEGER NOT NULL CHECK (schema_version >= 1),
    workspace_id TEXT NOT NULL CHECK (length(workspace_id) > 0)
) STRICT;

CREATE TRIGGER baukfs_metadata_single_row
BEFORE INSERT ON baukfs_metadata
WHEN EXISTS (SELECT 1 FROM baukfs_metadata)
BEGIN SELECT RAISE(ABORT, 'baukfs_metadata holds exactly one row'); END;

CREATE TRIGGER baukfs_metadata_identity_immutable
BEFORE UPDATE OF workspace_id ON baukfs_metadata
WHEN NEW.workspace_id IS NOT OLD.workspace_id
BEGIN SELECT RAISE(ABORT, 'baukfs workspace identity is immutable'); END;

CREATE TRIGGER baukfs_metadata_undeletable
BEFORE DELETE ON baukfs_metadata
BEGIN SELECT RAISE(ABORT, 'baukfs_metadata row cannot be deleted'); END;

INSERT INTO baukfs_metadata (singleton, schema_version, workspace_id) VALUES (1, 1, ?workspace_id);
```

- `singleton = 1` together with the primary key allows at most one row, so a database holds exactly one workspace identity.
- The insert trigger runs before conflict resolution, so it also stops `INSERT OR REPLACE` from swapping the identity row.
- The identity cannot be updated or deleted through SQL. `schema_version` stays updatable for future upgrades.
- `workspace_id` is stored exactly as `WorkspaceId::as_str()` returns it. As described in `docs/domain-contract.md`, it is metadata and is never used to build a path.
- Turso 0.8.3 reports a trigger's `RAISE(ABORT)` as its generic `Error` variant (`DriverErrorKind::Other`), not `Constraint`.

No content, manifest, revision, publication, or runner tables exist yet.

### Readiness classification

`inspect`, and the checks inside `create` and `open_existing`, classify a database by reading `sqlite_schema` and `baukfs_metadata`. They never write.

1. The file does not exist: `Missing`.
2. The driver reports the file is not a database: `Unrecognized(NotADatabase)`.
3. No schema objects at all: `Uninitialized`. This covers a zero-byte file and an empty database the driver created.
4. Schema objects but no `baukfs_metadata` table: `Unrecognized(ForeignSchema)`.
5. Not exactly one metadata row, or a non-integer version: `Unrecognized(MalformedMetadata)`.
6. A version other than `CURRENT_SCHEMA_VERSION`, older or newer: `UnsupportedSchemaVersion { found }`.
7. Any version-1 object missing: `Unrecognized(IncompleteSchema)`.
8. An identity that is not text or not a valid `WorkspaceId`: `Unrecognized(MalformedMetadata)`.
9. Otherwise: `Current(workspace_id)`.

Extra objects in a current database are tolerated.

### Future schema upgrades

Not implemented. Later steps must follow these rules:

- Versions are consecutive integers. Each upgrade is one explicit step from version `N` to `N + 1`.
- Upgrades run only from a dedicated, explicit upgrade operation, never from `inspect` or `open_existing`. Those keep reporting `UnsupportedSchemaVersion` until the upgrade has run.
- The upgrade operation applies every pending step in ascending order inside one `BEGIN IMMEDIATE` transaction. It rechecks the version after taking the lock and sets `schema_version` to the final version in the same transaction, so a database is never left between versions.
- `create` builds a new database with the version-1 bootstrap and then the same ordered steps in its single transaction. New and upgraded databases therefore share one schema path.
- Downgrades are not supported. A version newer than the binary understands is always `UnsupportedSchemaVersion`.
- `CURRENT_OBJECTS` in `schema.rs` must list every object the current version requires.

## Creation versus existing-only opening

| State at `path` | `inspect` | `open_existing(id)` | `create(id)` |
| --- | --- | --- | --- |
| Missing (parent exists) | `Missing`, no files created | `Err(Missing)`, no files created | Creates the file, initializes it: `Created` |
| Missing parent directory | `Missing`, nothing created | `Err(Missing)`, nothing created | `Err(Driver { step: Open, kind: Io(NotFound) })`, nothing created |
| Uninitialized (empty) | `Uninitialized` | `Err(Uninitialized)`, no writes | Initializes: `Created`, or `InitializedConcurrently` |
| Current for `id` | `Current(id)` | Opens | Opens: `AlreadyInitialized` |
| Current for another id | `Current(other)` | `Err(WorkspaceMismatch)` | `Err(WorkspaceMismatch)`, identity untouched |
| Unsupported version | `UnsupportedSchemaVersion { found }` | `Err(UnsupportedSchemaVersion)`, no writes | `Err(UnsupportedSchemaVersion)`, no writes |
| Foreign schema | `Unrecognized(ForeignSchema)` | `Err(Unrecognized)`, no writes | `Err(Unrecognized)`, no writes |
| Not a database | `Unrecognized(NotADatabase)` | `Err(Unrecognized)`, file untouched | `Err(Unrecognized)`, file untouched |
| Non-UTF-8 path | `Err(NonUtf8Path)` | `Err(NonUtf8Path)` | `Err(NonUtf8Path)` |

How each operation works:

- **`inspect`** uses a read-only open. A read-only open never creates the database, its parent directory, or sidecar files, and takes no write lock.
- **`open_existing`** inspects first and opens read-write only if the database is current for the requested workspace. It then re-reads the state on the read-write connection before returning the store. It never runs DDL, writes metadata, or upgrades.
- **`create`** inspects first, so it never opens an unsupported, foreign, or non-database file read-write. If the database is already current for the workspace, it opens it without taking a write lock. Otherwise it opens read-write, which creates the file when absent, and initializes it as described in [Initialization atomicity](#initialization-atomicity-and-contention).
- **Parent directories.** `create` does not create them. The caller places the database, and it must be **outside the agent-writable working directory** on a local filesystem. Turso refuses multiprocess WAL on network filesystems.
- **Read-write opens can create files.** The driver offers no flag to open read-write without creating. If a file is deleted between the read-only check and the read-write open, the read-write open recreates it empty and the re-read rejects it as `Uninitialized`. Deleting a live workspace database is outside this contract.

A store handle is returned only after the read-write connection has read the database as `Current` for the requested workspace.

## Multiprocess mode and wait policy

- **One open path.** Every open goes through the same internal `open`, which always sets `experimental_multiprocess_wal(true)` (see constraint 1 in `docs/turso-compatibility.md`). No API disables it. Opens differ only in read-only versus read-write access.
- **Busy timeout per connection.** Every connection gets the caller's `BusyTimeout` as soon as it is created, for read-only connections as well.
- **Coordination-file race at first open.** The first multiprocess opener of a database creates its `-tshm` coordination file and locks it before sizing and initializing it. Turso 0.8.3 reports `Corrupt` ("smaller than the coordination header" or "magic mismatch") to any opener, read-only or read-write, that arrives in that window. The parallel-creator tests reproduced this in about half of their runs before the fix. The open helper retries an open that fails with the `Corrupt` variant until the busy timeout elapses, then returns `Driver { step: Open, kind: Corrupt }`. With `BusyTimeout::IMMEDIATE` it makes one attempt. Only the variant is matched, never the message. A database that is really corrupt still fails, but only after the timeout.
- **The busy timeout is not a deadline.** It bounds the driver's wait for one contended lock, and the open retry above. One operation can wait once for each lock it acquires, and under load a wait has overrun its timeout by seconds. The driver also spins a CPU core while it waits. Callers that need a bound on a whole operation must enforce it themselves.
- **Contention is typed.** `DriverErrorKind::Busy` and `BusySnapshot` come from matching the driver's error variant, and `DriverErrorKind::is_busy()` covers both. No storage code reads diagnostic messages. A missing file is detected from `IoError(NotFound)` at the open step, and a non-database file from the `NotAdb` variant.

## Initialization atomicity and contention

Initialization runs on one connection, which `create` borrows mutably:

1. Begin a checked `BEGIN IMMEDIATE` transaction (`Connection::transaction_with_behavior`, which needs `&mut Connection`).
2. Recheck the state under the write lock:
   - `Uninitialized`: apply the bootstrap steps and insert the metadata row.
   - `Current` for the same workspace: another initializer won. Roll back and return `InitializedConcurrently`.
   - Anything else: roll back and return the matching error (`WorkspaceMismatch`, `UnsupportedSchemaVersion`, `Unrecognized`).
3. Read the state back inside the transaction. Commit only if it is `Current` for the requested workspace. Otherwise roll back.
4. Read the state again on the now-autocommit connection before returning the store.

Every path ends in an explicit `COMMIT` or `ROLLBACK`.

- The schema and the metadata row are created in one transaction, so they become visible together or not at all.
- A failed bootstrap step is returned as `Driver { step: Bootstrap, .. }` after a successful rollback.
- If the rollback itself fails, `RollbackFailed { abandoned, rollback }` keeps both errors. `abandoned` is either `Found(DatabaseState)` (the state seen under the lock) or `Failed(DriverError)`. Both enums are flat, so no error nests another `StorageError`.
- On any error, including a failed `COMMIT`, the connection is dropped rather than returned. After a failed `COMMIT` the outcome is uncertain, and the caller should run `inspect`.

Concurrent initializers:

- **Same identity:** exactly one commits (`Created`). The others report `InitializedConcurrently` or `AlreadyInitialized`, depending on whether they passed the read-only check before or after the commit.
- **Different identities:** exactly one commits. Every other initializer reports `WorkspaceMismatch { requested: own, stored: winner }`. The stored identity is never overwritten. Triggers also enforce this in SQL.
- **Contention:** a writer that cannot get the lock within its busy timeout fails with `Driver { step: Begin, kind: Busy | BusySnapshot }` and leaves the database unchanged.

## Tests

Unit tests in `crates/baukfs/src/storage/tests.rs`:

- An injected bootstrap failure: a second metadata insert, rejected by the single-row trigger, appended after the real steps. The test checks that schema and metadata roll back together and that a later initialization succeeds.
- A deterministic recheck under the lock, for the same identity and for a different identity.

Integration tests in `crates/baukfs/tests/storage/`:

- `lifecycle.rs`: create and reopen, idempotent creation, identity rejection, SQL-level identity constraints, missing parent directory, non-UTF-8 paths.
- `existing.rs`: missing databases leave the directory unchanged; inspection of a current database leaves its entries, database file, and WAL byte-identical; `open_existing` leaves the schema cookie and metadata unchanged; uninitialized, unsupported-version, foreign-schema, and non-database outcomes.
- `concurrency.rs`: separate processes create the database in parallel, with one identity and with four different identities; creators queue behind a lock held by another process; inspection, existing-open, and create of a current database run while another process holds an uncommitted write; typed `Busy` at the begin step with a 0 ms and a 300 ms busy timeout.

The `create` children wait at a start barrier (`ready`, then `go` on stdin), so the parallel creators really overlap. That overlap is what exposed the coordination-file race.

Process tests re-run the test binary as a child (`child_process_entry`, chosen by environment variables), so they need no extra binary crate. The harness (`harness.rs`), adapted from the T03 spike's, gives each child one deadline (1 minute) covering every line and its exit. It reads stdout and stderr on separate threads, kills and reaps the child on failure or drop, and reports typed errors that include the child's stderr. The only sleep is the 250 ms hold in the queued-creator tests. The assertions do not depend on it: while the lock is held, no creator may have finished, and the outcome invariants hold however the waiters are scheduled.

## Deferred responsibilities

- **T06–T08:**
  - Tables and operations for file contents, manifests, and revisions, added as numbered upgrade steps.
  - Revision persistence that is atomic with workspace transitions.
  - Crash recovery and the process-kill matrix for revision commits.
  - Any write path on `WorkspaceStore`.
- **Operation deadlines and workspace serialization.** Multiprocess WAL protects the file, but it does not serialize workspace-mutating operations.
- **Choosing database paths.** Keeping them outside the agent-writable working directory is the caller's job; storage only receives a path.
- **Platforms.** All storage tests have run only on x86_64-linux. aarch64-linux and aarch64-darwin are compile and evaluation targets until they are run natively (see `docs/turso-compatibility.md`, Remaining platform verification).
- **Durability.** `PRAGMA fullfsync` on macOS, and durability across power loss and OS crashes, remain open from T03.
