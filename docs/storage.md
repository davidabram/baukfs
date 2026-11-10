# Workspace storage (T05–T08)

`baukfs::storage` stores one independent workspace copy per local Turso database: immutable contents, canonical manifests, immutable revisions and lineage, and the workspace session/base/head/status. Database initialization and history initialization are separate operations. Schema version 3 can be current with no history.

The domain modules (`WorkspaceId`, `ContentId`, `Workspace`, …) have no database code. Storage depends on them; they do not depend on storage.

## Public API

| Item | Purpose |
| --- | --- |
| `WorkspaceStore::create(path, &WorkspaceId, BusyTimeout)` | Explicit creation. Initializes an absent or empty database for the given workspace, or opens it if it is already initialized for that workspace. Returns `(WorkspaceStore, CreateOutcome)`. |
| `WorkspaceStore::open_existing(path, &WorkspaceId, BusyTimeout)` | Opens an existing database for normal use. It must be current and belong to the given workspace. |
| `WorkspaceStore::upgrade(path, &WorkspaceId, BusyTimeout)` | Explicit schema upgrade. Brings an older database for the given workspace to the current version and opens it. Returns `(WorkspaceStore, UpgradeOutcome)`. |
| `storage::inspect(path, BusyTimeout)` | Read-only inspection. Returns a `DatabaseState`. |
| `WorkspaceStore::{workspace_id, busy_timeout, read_state}` | The stored identity, the connection's wait policy, and a re-read of the state through the store's own connection. |
| `WorkspaceStore::insert_content(&mut self, &[u8])` | Stores bytes under their `ContentId`. Returns `(ContentId, InsertOutcome)`. |
| `WorkspaceStore::read_content(&self, &ContentId)` | Returns `Ok(Some(bytes))`, `Ok(None)` when nothing is stored under the identifier, or an error. |
| `CreateOutcome` | `Created`, `AlreadyInitialized` (found by the read-only check, no write lock taken), or `InitializedConcurrently` (another initializer for the same workspace committed while this call waited for the lock). |
| `UpgradeOutcome` | `Upgraded { from }`, `AlreadyCurrent` (found by the read-only check, no write lock taken), or `UpgradedConcurrently` (another upgrader committed while this call waited for the lock). |
| `InsertOutcome` | `Inserted` or `AlreadyPresent`. |
| `DatabaseState` | `Missing`, `Uninitialized`, `Current(WorkspaceId)`, `UpgradeRequired { found, workspace_id }`, `UnsupportedSchemaVersion { found }`, or `Unrecognized(Unrecognized)`. |
| `Unrecognized` | `NotADatabase`, `ForeignSchema` (schema objects present, no BaukFS metadata table), `IncompleteSchema` (known version but objects it requires are missing), `MalformedMetadata` (not exactly one well-formed metadata row). |
| `BusyTimeout` | Per-connection lock-wait policy. `BusyTimeout::IMMEDIATE` or `BusyTimeout::new(Duration)`. |
| `StorageError` | `ConnectionDiscarded`, `History(HistoryError)`, `NonUtf8Path`, `Driver(DriverError)`, `Missing`, `Uninitialized`, `UpgradeRequired { found }`, `UnsupportedSchemaVersion { found }`, `Unrecognized`, `WorkspaceMismatch { requested, stored }`, `Integrity(IntegrityError)`, `RollbackFailed { abandoned, rollback }`. |
| `IntegrityError` | The `ContentId` and the `IntegrityViolation`: `MalformedRecord`, `LengthMismatch { recorded, stored }`, `DigestMismatch { computed }`, or `ConflictingContents`. Its fields are private. |
| `Abandoned` | Why a write transaction was rolled back: `Found(DatabaseState)`, `Failed(DriverError)`, or `Integrity(IntegrityError)`, or `History(HistoryError)`. |
| `DriverError` | The failing `DriverStep` and a `DriverErrorKind`, classified from the driver's error variant. Its fields are private. |
| `CURRENT_SCHEMA_VERSION` | `3`. |

All operations are `async fn`s that drive the driver's own futures. The library does not own a runtime, spawn threads, or use blocking wrappers. Callers supply the executor. The tests use a small spinning `block_on`.

`WorkspaceStore` holds one private `turso::Connection`. There are no backend traits, pools, or erased types.

## Content identity

`baukfs::ContentId` identifies an exact byte sequence. It is a newtype over the fixed 32-byte BLAKE3 digest of the bytes (`blake3` 1.8.7, declared once in the workspace manifest). Its field is private.

- `ContentId::of(&[u8])` hashes the bytes exactly as given. Nothing is normalized: line endings, encodings, and trailing bytes all count, bytes are never decoded as text, and no path or file name takes part in the hash.
- The canonical text form is `b3:` followed by 64 lowercase hexadecimal digits, for example `b3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262` for the empty input. `Display` writes it, and `ContentId::parse` (also `FromStr`) reads it.
- Parsing accepts only the canonical form, so every identifier has exactly one spelling. It reports the first defect as a `ContentIdError`: `MissingPrefix` (including `B3:`), `InvalidCharacter { offset, character }` for anything other than `0-9a-f` after the prefix, with a byte offset into the whole text (uppercase digits are rejected), or `InvalidLength { digits }` for anything but 64 digits.
- `digest()` borrows the 32 digest bytes. Storage keys records by these bytes, not by the text form.
- `ContentId` is distinct from `WorkspaceId` and `RevisionId`. It is computed, not chosen, and is not an opaque caller-supplied string.

## Database identity and bootstrap schema

Version 1 consists of one table and three triggers:

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

## Schema version 2: content

The upgrade from version 1 to 2 adds one table and two triggers:

```sql
CREATE TABLE baukfs_content (
    digest BLOB PRIMARY KEY NOT NULL CHECK (length(digest) = 32),
    byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
    contents BLOB NOT NULL
) STRICT;

CREATE TRIGGER baukfs_content_insert_once
BEFORE INSERT ON baukfs_content
WHEN EXISTS (SELECT 1 FROM baukfs_content WHERE digest = NEW.digest)
BEGIN SELECT RAISE(ABORT, 'baukfs content is stored once per digest'); END;

CREATE TRIGGER baukfs_content_immutable
BEFORE UPDATE ON baukfs_content
BEGIN SELECT RAISE(ABORT, 'baukfs content is immutable'); END;
```

- One row holds one whole file: the 32 raw digest bytes, the byte length, and the exact bytes. There is no compression, chunking, deduplication across databases, or reference counting.
- The primary key keeps digests unique. Because the insert trigger runs before conflict resolution (as for the metadata table), `INSERT OR REPLACE` cannot swap a record, and the update trigger blocks every `UPDATE`. A record, once stored, is never changed through SQL.
- Deletion is not blocked, and no API deletes. Garbage collection is out of scope.
- `byte_length` is deliberately not tied to `length(contents)` by a `CHECK`. Reads compare the two, so a damaged record is reported as an integrity error rather than being impossible to represent in tests.

Version 3 adds manifests, revisions, and workspace state as described below. Publication and runner tables remain deferred.

### Readiness classification

`inspect`, and the checks inside `create`, `open_existing`, and `upgrade`, classify a database by reading `sqlite_schema` and `baukfs_metadata`. They never write.

1. The file does not exist: `Missing`.
2. The driver reports the file is not a database: `Unrecognized(NotADatabase)`.
3. No schema objects at all: `Uninitialized`. This covers a zero-byte file and an empty database the driver created.
4. Schema objects but no `baukfs_metadata` table: `Unrecognized(ForeignSchema)`.
5. Not exactly one metadata row, or a non-integer version: `Unrecognized(MalformedMetadata)`.
6. A version newer than `CURRENT_SCHEMA_VERSION`, or below 1: `UnsupportedSchemaVersion { found }`.
7. Any object that version requires is missing: `Unrecognized(IncompleteSchema)`. Version 1 requires the metadata table and its three triggers. Version 2 also requires the content table and its two triggers. Version 3 additionally requires its three history tables and nine triggers.
8. An identity that is not text or not a valid `WorkspaceId`: `Unrecognized(MalformedMetadata)`.
9. An older known version: `UpgradeRequired { found, workspace_id }`.
10. Otherwise: `Current(workspace_id)`.

Extra objects are tolerated.

Steps 5 to 10 read the object list and the metadata row in **one** `SELECT … UNION ALL SELECT …` statement, so both come from the same committed snapshot. With two separate statements, an inspector racing an upgrade could pair the pre-upgrade object list with the post-upgrade version and misreport the database as `IncompleteSchema`. Running the parallel-upgrade test with two statements reproduced exactly that in 4 of 6 runs. Steps 3 and 4 use an earlier statement, which is safe because neither initialization nor an upgrade removes the metadata table.

## Schema upgrades

Rules:

- Versions are consecutive integers. Each upgrade step goes from version `N` to `N + 1`. `UPGRADES` in `schema.rs` lists the steps in order, and `INTRODUCED_OBJECTS` lists the objects each version adds. A compile-time assertion and a unit test keep both in line with `CURRENT_SCHEMA_VERSION`.
- Only `WorkspaceStore::upgrade` migrates. `inspect`, `open_existing`, and `create` never do. For an older database they report `UpgradeRequired` and leave the file, WAL, schema cookie, and metadata unchanged.
- Downgrades are not supported. A version newer than the binary understands is always `UnsupportedSchemaVersion`.
- `create` applies the version-1 bootstrap and then every upgrade step, in the same order, within its single initialization transaction, and then sets `schema_version` to the current version. New and upgraded databases share one schema path.

How `upgrade` works:

1. **Read-only check.** `inspect` first. `UpgradeRequired` for the requested workspace continues. `Current` for it opens the store without a write lock and returns `AlreadyCurrent`. Anything else returns its error without opening read-write: `Missing` (nothing created), `Uninitialized`, `UnsupportedSchemaVersion`, `Unrecognized`, or `WorkspaceMismatch` when the old database belongs to another workspace.
2. **Lock and recheck.** It opens read-write and begins a checked `BEGIN IMMEDIATE` transaction, then classifies again under the write lock. `UpgradeRequired` for the workspace continues. `Current` for it means another upgrader won: roll back and return `UpgradedConcurrently`. Anything else rolls back and returns the matching error.
3. **Apply.** It applies every pending step in ascending order and sets `schema_version` to the current version, all in that transaction. A failed step is returned as `Driver { step: Upgrade, .. }` after a successful rollback, and the database stays at its old version.
4. **Verify and commit.** It commits only if the state reads back as `Current` for the workspace, and reads it again on the autocommit connection before returning the store.

The identity row is never rewritten. Its triggers stay in place, and only `schema_version` changes. Rollback failures are kept as `RollbackFailed { abandoned, rollback }`, as for initialization. On any error the connection is dropped.

## Content storage

`insert_content(&mut self, bytes)`:

1. Computes `ContentId::of(bytes)` before touching the database.
2. Begins a checked `BEGIN IMMEDIATE` transaction. Taking the write lock before reading avoids `BusySnapshot`, the error for a read snapshot that cannot be upgraded to a write, and ensures that no other process can insert the same digest between the lookup and the insert.
3. Looks the digest up. If a record exists, it is verified as on a read (step 1 below) and compared byte for byte with `bytes`. Identical bytes return `AlreadyPresent` without writing. Any disagreement rolls back and returns `Integrity`. The existing record is never overwritten.
4. Otherwise it inserts `(digest, length(?2), ?2)` with bound parameters, so SQL computes the length from the same bound value, and commits. The result is `Inserted`.

`&mut self` reflects the checked transaction, which needs exclusive use of the connection. Every value is passed as a bound parameter. The raw connection stays private.

`read_content(&self, id)` runs one `SELECT` in autocommit mode:

- No row: `Ok(None)`. This is the only way a missing record is reported. Failures are always `Err`.
- Otherwise the record is verified before any byte is returned:
  1. Not an integer length plus a BLOB: `Integrity(MalformedRecord)`.
  2. `byte_length` differs from the stored BLOB's length: `Integrity(LengthMismatch { recorded, stored })`.
  3. The recomputed BLAKE3 digest of the stored bytes differs from `id`: `Integrity(DigestMismatch { computed })`.

`ConflictingContents` means the stored record is internally consistent, yet its bytes differ from the bytes being inserted. Only a BLAKE3 collision could produce it.

Insertion is idempotent and safe across processes. Parallel inserters of the same bytes serialize on the write lock. Exactly one records `Inserted`, and every other one records `AlreadyPresent`.

If `COMMIT` fails during an insert, the outcome is uncertain. T08 discards the connection and leaves the handle unusable. Reopen, verify the content, and retry the idempotent insert if necessary; never depend on a dangling transaction being cleaned up by the next operation.

## Creation versus existing-only opening

| State at `path` | `inspect` | `open_existing(id)` | `create(id)` |
| --- | --- | --- | --- |
| Missing (parent exists) | `Missing`, no files created | `Err(Missing)`, no files created | Creates the file, initializes it: `Created` |
| Missing parent directory | `Missing`, nothing created | `Err(Missing)`, nothing created | `Err(Driver { step: Open, kind: Io(NotFound) })`, nothing created |
| Uninitialized (empty) | `Uninitialized` | `Err(Uninitialized)`, no writes | Initializes: `Created`, or `InitializedConcurrently` |
| Current for `id` | `Current(id)` | Opens | Opens: `AlreadyInitialized` |
| Current for another id | `Current(other)` | `Err(WorkspaceMismatch)` | `Err(WorkspaceMismatch)`, identity untouched |
| Older version for `id` | `UpgradeRequired { found, id }` | `Err(UpgradeRequired)`, no writes | `Err(UpgradeRequired)`, no writes |
| Older version for another id | `UpgradeRequired { found, other }` | `Err(WorkspaceMismatch)`, no writes | `Err(WorkspaceMismatch)`, no writes |
| Unsupported version | `UnsupportedSchemaVersion { found }` | `Err(UnsupportedSchemaVersion)`, no writes | `Err(UnsupportedSchemaVersion)`, no writes |
| Foreign schema | `Unrecognized(ForeignSchema)` | `Err(Unrecognized)`, no writes | `Err(Unrecognized)`, no writes |
| Not a database | `Unrecognized(NotADatabase)` | `Err(Unrecognized)`, file untouched | `Err(Unrecognized)`, file untouched |
| Non-UTF-8 path | `Err(NonUtf8Path)` | `Err(NonUtf8Path)` | `Err(NonUtf8Path)` |

How each operation works:

- **`inspect`** uses a read-only open. A read-only open never creates the database, its parent directory, or sidecar files, and takes no write lock.
- **`open_existing`** inspects first and opens read-write only if the database is current for the requested workspace. It then re-reads the state on the read-write connection before returning the store. It never runs DDL, writes metadata, or upgrades.
- **`upgrade`** is covered in [Schema upgrades](#schema-upgrades). Like `open_existing`, it never creates a missing database.
- **`create`** inspects first, so it never opens an older, unsupported, foreign, or non-database file read-write. If the database is already current for the workspace, it opens it without taking a write lock. Otherwise it opens read-write, which creates the file when absent, and initializes it as described in [Initialization atomicity](#initialization-atomicity-and-contention).
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

Initialization, upgrades, content insertion, and history writes share one checked helper that runs work under a checked `BEGIN IMMEDIATE` transaction (`Connection::transaction_with_behavior`, which needs `&mut Connection`). The helper attempts an explicit `COMMIT` or `ROLLBACK` on every completed path. History reads use the same checked boundary with `BEGIN DEFERRED` for one consistent snapshot.

Initialization runs on one connection, which `create` borrows mutably:

1. Begin the transaction.
2. Recheck the state under the write lock:
   - `Uninitialized`: apply the version-1 bootstrap, which inserts the metadata row at version 1, then every upgrade step, then set `schema_version` to the current version.
   - `Current` for the same workspace: another initializer won. Roll back and return `InitializedConcurrently`.
   - Anything else: roll back and return the matching error (`WorkspaceMismatch`, `UpgradeRequired`, `UnsupportedSchemaVersion`, `Unrecognized`).
3. Read the state back inside the transaction. Commit only if it is `Current` for the requested workspace. Otherwise roll back.
4. Read the state again on the now-autocommit connection before returning the store.

Every path ends in an explicit `COMMIT` or `ROLLBACK`.

- The schema and the metadata row are created in one transaction, so they become visible together or not at all.
- A failed bootstrap step is returned as `Driver { step: Bootstrap, .. }` after a successful rollback.
- If the rollback itself fails, `RollbackFailed { abandoned, rollback }` keeps both errors. `abandoned` is `Found(DatabaseState)` (the state seen under the lock), `Failed(DriverError)`, or `Integrity(IntegrityError)` (inserts only). These enums are flat, so no error nests another `StorageError` and nothing is boxed.
- On any error, including a failed `COMMIT`, the connection is dropped rather than returned. After a failed `COMMIT` the outcome is uncertain. Inspect schema state for schema writes; reopen and read validated workspace history for history writes.

Concurrent initializers:

- **Same identity:** exactly one commits (`Created`). The others report `InitializedConcurrently` or `AlreadyInitialized`, depending on whether they passed the read-only check before or after the commit.
- **Different identities:** exactly one commits. Every other initializer reports `WorkspaceMismatch { requested: own, stored: winner }`. The stored identity is never overwritten. Triggers also enforce this in SQL.
- **Contention:** a writer that cannot get the lock within its busy timeout fails with `Driver { step: Begin, kind: Busy | BusySnapshot }` and leaves the database unchanged.

## Tests

Unit tests in `crates/baukfs/src/storage/tests.rs`:

- An injected bootstrap failure: a second metadata insert, rejected by the single-row trigger, appended after the real creation steps. The test checks that schema and metadata roll back together and that a later initialization succeeds.
- A deterministic recheck under the lock, for the same identity and for a different identity.
- An injected upgrade failure: the same failing step appended to the version-2 upgrade. The database stays at version 1 with its original objects, and a later upgrade succeeds.
- A deterministic upgrade recheck: winner `Upgraded { from: 1 }`, a late upgrader `UpgradedConcurrently`, another identity `WorkspaceMismatch`.
- The schema tables agree with `CURRENT_SCHEMA_VERSION`.

`crates/baukfs/tests/content_id.rs`: the BLAKE3 reference vector for the empty input, distinct identifiers for inputs that differ only in line endings, case, or a trailing NUL, canonical round trips, and rejection of malformed text (missing or uppercase prefix, wrong digit counts, uppercase digits, non-hex and non-ASCII characters, whitespace).

Integration tests in `crates/baukfs/tests/storage/`:

- `lifecycle.rs`: create and reopen, idempotent creation, identity rejection, SQL-level identity constraints, missing parent directory, non-UTF-8 paths.
- `existing.rs`: missing databases leave the directory unchanged; inspection of a current database leaves its entries, database file, and WAL byte-identical; `open_existing` leaves the schema cookie and metadata unchanged; uninitialized, unsupported-version, foreign-schema, and non-database outcomes.
- `content.rs`: round trips for empty bytes, embedded NUL, non-UTF-8 bytes, all 256 byte values, and an 8 MiB payload, each under its BLAKE3 identifier, with distinct identifiers; repeated inserts through two handles keep one record; missing content is `None`; bytes persist across reopen, and reads leave the schema cookie and records unchanged; two copy databases store and read content independently; SQL rejects `UPDATE`, `INSERT OR REPLACE`, duplicate digests, short digests, negative lengths, and `NULL` contents; tampered bytes give `DigestMismatch` and a tampered length gives `LengthMismatch` on both read and re-insert, and the tampered record is not overwritten.
- `upgrade.rs`: version-1 fixtures (frozen copies of the version-1 DDL) are reported as `UpgradeRequired`, and `inspect`, `open_existing`, and `create` leave their file, WAL, objects, metadata, and schema cookie unchanged; a successful upgrade preserves the identity and its triggers, enables content, and a repeat reports `AlreadyCurrent`; `upgrade` rejects another identity, version 99, a foreign schema, incomplete version-1 and version-2 schemas, a non-database file, and an empty database without changing them, and does not create a missing one; a version-1 database that already has an unrelated `baukfs_content` table fails at `DriverStep::Upgrade` and stays at version 1 with that table intact.
- `concurrency.rs`: separate processes create the database in parallel, with one identity and with four different identities; creators queue behind a lock held by another process; inspection, existing-open, and create of a current database run while another process holds an uncommitted write; typed `Busy` at the begin step with a 0 ms and a 300 ms busy timeout. T06 adds four parallel upgrader processes with two inspector processes, run both freely and queued behind a held lock. Exactly one upgrader reports `Upgraded { from: 1 }`, the others `AlreadyCurrent` or `UpgradedConcurrently`. Every inspector sees only `UpgradeRequired` and then `Current`. It also adds four parallel inserter processes storing the same 16 payloads (0 to about 68 KiB): each payload is `Inserted` exactly once, and the table ends with exactly those 16 records.

The `create` children wait at a start barrier (`ready`, then `go` on stdin), so the parallel creators really overlap. That overlap is what exposed the coordination-file race.

Process tests re-run the test binary as a child (`child_process_entry`, chosen by environment variables), so they need no extra binary crate. Child roles are `create`, `upgrade`, `insert`, `advance-a`, `advance-b`, `observe` (inspect in a loop until current and report each distinct state), and `hold`. The harness (`harness.rs`), adapted from the T03 spike's, gives each child one deadline (1 minute) covering every line and its exit. It reads stdout and stderr on separate threads, kills and reaps the child on failure or drop, and reports typed errors that include the child's stderr. The only sleep is the 250 ms hold in the queued-creator and queued-upgrader tests. The assertions do not depend on it: while the lock is held, no creator may have finished, and the outcome invariants hold however the waiters are scheduled.

## Schema version 3 and history API (T08)

The ordered 2→3 upgrade adds:

```sql
CREATE TABLE baukfs_manifests (
    digest BLOB PRIMARY KEY NOT NULL CHECK (length(digest) = 32),
    encoding BLOB NOT NULL
) STRICT;
CREATE TABLE baukfs_revisions (
    id TEXT PRIMARY KEY NOT NULL,
    manifest BLOB NOT NULL CHECK (length(manifest) = 32),
    parent TEXT
) STRICT;
CREATE TABLE baukfs_workspace (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    session TEXT NOT NULL,
    base TEXT NOT NULL,
    head TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('idle', 'running', 'sealed'))
) STRICT;
```

Each history table has `insert_once`, `immutable`, and `undeletable` triggers, named `<table>_<suffix>`. Insert triggers reject a duplicate key before conflict resolution, including `INSERT OR REPLACE`. Manifest and revision updates and deletes are rejected. The workspace update trigger rejects updates of `session`, `base`, or `singleton`; its head and status columns remain writable. No public API changes persisted status in T08. The workspace identity remains in the unchanged metadata row. No foreign-key enforcement is assumed: reference checks run at the application boundary inside the transaction.

Fresh creation uses bootstrap→2→3. Upgrade from version 2 preserves identity and content and creates empty history tables. Inspection, existing-only opening, and `create` against an old database remain non-migrating.

| Public API | Result / behavior |
| --- | --- |
| `initialize_history(&mut self, &SessionId, &Manifest)` | `(Workspace, HistoryOutcome::Initialized)` after commit, or `AlreadyApplied` for an identical existing idle root. |
| `advance_head(&mut self, &RevisionId, &Manifest)` | `(Workspace, HistoryOutcome::Advanced)` after commit, or a verified `AlreadyApplied` retry. |
| `read_workspace(&mut self)` | `Option<Workspace>`; `None` means history is not initialized. |
| `read_revision(&mut self, &RevisionId)` | `Option<Revision>`; validates the record and complete ancestry. |
| `read_manifest(&mut self, &ManifestId)` | `Option<Manifest>`; validates canonical encoding, identity, and contents. |
| `Revision::{id, manifest, parent}` | Borrowed access to private immutable fields. No public revision constructor or setters. |
| `HistoryError` | Typed missing references, malformed records, manifest decoding/identity and revision identity failures, conflicting records, invalid lineage, initialization conflicts, lifecycle transitions, and stale heads with expected/actual IDs. Wrapped by `StorageError::History` and `Abandoned::History`. |
| `StorageError::ConnectionDiscarded` | Reopen and reconcile before doing more database work on this handle. |

The read methods need exclusive connection access for a checked `BEGIN DEFERRED` snapshot. They do not write records or schema. Missing requested objects return `None`; missing required references return typed errors. Content failures reuse existing `IntegrityError` and its length/digest classifications. A repeated content digest within a manifest is checked once. Revision reads iteratively verify every parent and its manifest/content references; workspace reads additionally require that the ultimate root equals the persisted base. A visited-ID set rejects cycles without recursive or erased futures.

### Revision identity: encoding version 1

The identity preimage is the following exact concatenation, with no padding or terminator beyond the stated domain NUL:

| Field | Encoding |
| --- | --- |
| Domain | 16 bytes, ASCII `baukfs-revision` followed by one NUL |
| Version | u32 little-endian, value 1 |
| Manifest | 32 raw ManifestId digest bytes |
| Parent presence | One byte: 0 for root, 1 for a child |
| Parent, only when present | UTF-8 byte length as u64 little-endian, then the exact `RevisionId::as_str()` UTF-8 bytes |

The identifier is `rb3:` followed by the ordinary BLAKE3 hash of the entire preimage as 64 lowercase hexadecimal digits. A root preimage is exactly 53 bytes. There is no hidden derive-key context. The parent text is length-framed to retain the opaque domain API; store-generated parents have the canonical 68-byte spelling above. No clock, randomness, session, workspace identity, status, or diagnostic text participates. Identical histories can have identical IDs in independent databases; existence is local to each database. `RevisionId::new` keeps accepting opaque identifiers and does not prove storage existence. A golden framing test fixes both the root and child preimages.

### Initialization transaction

`initialize_history` acquires checked `BEGIN IMMEDIATE`, then reads and validates any existing workspace and history. An identical session, root manifest, base/head at that root, and Idle status returns `AlreadyApplied` after verifying all matching records. Any other existing initialized state returns `InitializationConflict`; initialization after head advancement is therefore a conflict, even with the original manifest and session.

For an uninitialized history it checks every distinct regular-file content reference using the content integrity verifier, persists or verifies the canonical manifest, persists or verifies the deterministic root revision, and inserts the workspace with `base == head == root` and `Idle`. All steps commit together. There is no Git import or filesystem scan.

### Head transaction and retries

`advance_head` acquires checked `BEGIN IMMEDIATE`, reads and validates persisted state, and uses T02's `record_checkpoint` transition on a private temporary value to require Idle. Running and Sealed reject advancement, including retries. It computes the target from the supplied manifest and expected head.

When current head equals expected, the operation validates contents and parent, persists or verifies the manifest and child revision, then executes:

```sql
UPDATE baukfs_workspace SET head = ?target
WHERE singleton = 1 AND head = ?expected AND status = 'idle';
```

Exactly one affected row is required; any other count returns `HeadCompareAndSwapFailed` and rolls back. Session and base remain unchanged. All values are bound parameters. The write lock covers validation, persistence, and the compare-and-swap. Two different targets from the same expected head cannot both advance it. A rejected operation rolls back any new manifest/revision and leaves existing contents untouched.

When current head differs from expected, only a current head equal to the computed target can return `AlreadyApplied`, after complete revision, parent, canonical manifest, and content verification and exact comparison. All other cases return `StaleHead`. Retrying the original request after a later revision has advanced returns a conflict. There are no persisted request receipts, so this is state-based retry recognition, not exactly-once request handling.

The caller receives the temporary `Workspace` only after commit. Caller-visible domain values are never mutated speculatively.

### Uncertain outcomes and cancellation

The store temporarily takes ownership of its private connection for content writes and history transactions. It returns the connection to the handle only after acknowledged commit or successful rollback. A begin/commit failure, rollback failure, or cancellation after taking the connection drops it and leaves the handle discarded. Subsequent storage operations fail with `ConnectionDiscarded` (identity and timeout accessors remain usable). `RollbackFailed` retains both typed reasons without boxing or erased error traversal. A failed commit never returns speculative success.

After any uncertain outcome, reopen with the same workspace identity and read the validated workspace/revision/manifest state. Retry using the original expected head and manifest: it can produce `Advanced`, a fully verified `AlreadyApplied`, or `StaleHead` if another writer has moved on. Schema `inspect` alone cannot reconcile a history commit. Driver power-loss durability remains a separate platform concern.

### T08 verification

History tests cover root initialization and reopening, multiple revisions preserving base/session/parents, manifest deduplication, missing and corrupted references, invalid encoding and identity, idempotent and conflicting initialization, stale heads and retries, Running/Sealed fixtures, independent databases, and version-2 upgrade with existing content. Trigger-injected failures before manifest insertion, revision insertion, workspace insertion, and head update leave no partial history. A zero-row compare-and-swap fixture also rolls back the new manifest/revision.

Two integration-test child processes start behind the existing bounded barrier and compete from one expected head: exactly one advances and the other reports stale. Unit process tests place a test-only checkpoint after persistence and before commit, or wait after the public API acknowledges commit. Both root initialization and advancement are killed at each boundary. Before commit, readers see only old state and reopening exposes none of the new manifest/revision/head; after acknowledged commit, reopening verifies both the revision and head. The unit harness uses the existing bounded-process techniques: concurrent stdout/stderr draining, a 30-second checkpoint deadline, and kill/reap on drop. Hooks are absent from non-test builds. Commit and rollback fault tests verify discarded handles and successful reconciliation through a fresh open.

## Deferred responsibilities

- **T09 and later:** filesystem scanning, restoration, cloning, runner integration, sealing, and publication. No new CLI commands are added by T08.
- **History validation cost.** Reads and head writes verify complete ancestry and its content references. There is no validation cache or application-level history quota; verification cost grows with history and content size.
- **Content storage limits.** Whole files are held in memory on insert and read, and stored as single BLOBs, with no streaming, chunking, compression, size limit beyond the driver's, or garbage collection. Manifests reference content, but raw SQL can still delete content. Reads and history writes reject missing references.
- **Operation deadlines and workspace serialization.** Multiprocess WAL plus `BEGIN IMMEDIATE` and an explicit head compare-and-swap serialize the implemented workspace writes. Whole-operation deadlines remain the caller’s responsibility.
- **Choosing database paths.** Keeping them outside the agent-writable working directory is the caller's job; storage only receives a path.
- **Platforms.** All storage tests, including T06's, have run only on x86_64-linux. aarch64-linux and aarch64-darwin are compile and evaluation targets until they are run natively (see `docs/turso-compatibility.md`, Remaining platform verification).
- **Durability.** `PRAGMA fullfsync` on macOS, and durability across power loss and OS crashes, remain open from T03.
