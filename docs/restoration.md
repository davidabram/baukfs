# Native revision restoration (T10)

`baukfs::restoration::restore_revision(&mut WorkspaceStore, &RevisionId, &Path)`
restores an explicitly selected stored revision to a new directory. The caller
supplies the executor. Filesystem operations block the calling thread. The
operation resolves that revision and its manifest through the existing validated
storage reads; it never selects or follows head. These reads verify canonical
encoding, identities, complete ancestry, and content references. Each file's
bytes are verified again immediately before writing. A concurrent head advance
does not change the chosen revision. Concurrent database corruption or deletion
can fail restoration; it cannot substitute unchecked bytes.

`RestoreReceipt` identifies the revision, manifest, absolute destination (with a
canonical parent), and directory, regular-file and symlink counts. Counts exclude
the implicit root. `RestoreError` contains a concrete `RestoreFailure` and an
optional cleanup error with the staging path. Missing revisions, missing content,
storage integrity/history failures, invalid/existing destinations, unavailable
publication, and filesystem operations have typed variants. Filesystem errors
retain the operation, path and native `io::Error`, including name collisions.

## Entry and permission policy

Raw Unix path components and symlink targets are preserved without text decoding,
normalization or case folding. Validated manifests require explicit parents.
Explicit directories, including empty ones, are created exclusively; every
regular-file path gets its own exclusively created file, even for equal content
IDs. Hard links are never created. Byte-distinct names that alias on the host
filesystem fail exclusive creation rather than merging. If the filesystem cannot
represent an entry, restoration fails before publication.

The staging root and initially created directories are created with mode 0700
(restricted further by umask), then explicitly set to 0700 before use. The root stays private throughout materialization.
Before publication, complete directories and the root receive exactly 0755;
regular files receive exactly 0644 or 0755 according to the executable flag.
These final modes are independent of umask. Symlink modes are the OS default
(and generally not meaningful on supported systems). No setuid, setgid, sticky
bits or other stored permission metadata are introduced. Ownership, timestamps,
ACLs, devices, sockets, FIFOs and hard-link identity are not restored. Native
ownership and timestamps are assigned by the OS. Parent ACL inheritance and
filesystem mount policy remain OS behavior; no ACL reconstruction is attempted.

## Containment and trusted parents

The destination parent must already exist and be a directory. The final component
must name a new entry, without a trailing slash or dot component. Relative parent
paths are resolved once to a canonical absolute parent. This API is scoped to
trusted destination-parent directories. It does **not** protect against a hostile
same-user process rewriting the parent or staging concurrently. It is not a
filesystem sandbox or protection against hostile mounts.

A fresh random 128-bit staging name is drawn from `/dev/urandom` beside the
destination and claimed with exclusive `mkdir`. Existing staging names are never
reused, and names do not depend on workspace identifiers. Only validated
workspace-relative paths are joined beneath that new root. Directories and
regular files are created before any manifest symlink. All directory permission
writes also finish before symlink creation. Absolute and parent-traversing targets
are written only as link data; no subsequent materialization writes follow them.
No restored file is executed.

## Publication and cleanup

Publication is a single atomic, no-replace namespace operation on the same
filesystem. Linux uses `renameat2` with `RENAME_NOREPLACE`; macOS uses `renamex_np`
with `RENAME_EXCL`, through pinned libc 0.2.191's native bindings. The added libc
dependency supplies concrete C ABI bindings and constants, with no application
backend, callbacks or erased-dispatch abstraction. The preliminary
`symlink_metadata` check improves diagnostics but is not the race protection.
A destination created immediately before publication wins, whether a file,
directory, symlink or dangling symlink. Unsupported kernel/filesystem operations
return `PublicationUnavailable` (ENOSYS, EINVAL or ENOTSUP); other native failures
retain their I/O diagnostic. There is no ordinary rename, copy or merge fallback.

On a prepublication failure, only this operation's exclusively owned staging is
removed. A concurrent destination is never deleted. If cleanup fails, the error
retains both the primary failure and the leftover staging path/error. Cancellation
or panic drops a staging guard and attempts best-effort removal. A process crash
may leave staging behind. There is no pattern-based or global scavenging.
Once rename succeeds the guard is disarmed and cannot delete the destination.
There are no fallible postpublication operations.

Atomic visibility is **not** a power-loss durability guarantee. This implementation
does not fsync files or directories, nor issue macOS fullfsync. Successful return
means complete namespace publication during normal OS operation; persistence
across OS crashes and power loss is not promised. No later durability error can
misreport a successful publication as absent because no later durability operation
is attempted.

One database still represents one workspace copy. Restoration performs reads only:
no schema change, head advance, provenance update or lifecycle transition. No
capture, fork/reflink, runner, Git publication or CLI behavior is added.

## Library example

Inside a caller-supplied async context, with a current store and existing trusted
parent directory:

```rust,no_run
# async fn example(store: &mut baukfs::storage::WorkspaceStore, session: &baukfs::SessionId)
# -> Result<(), ExampleError> {
let imported = baukfs::git_import::import_commit(
    std::path::Path::new("/trusted/repository"), "HEAD", session, store,
).await.map_err(ExampleError::Import)?;
let restored = baukfs::restoration::restore_revision(
    store, imported.origin.root_revision(),
    std::path::Path::new("/trusted/copies/new-copy"),
).await.map_err(ExampleError::Restore)?;
assert_eq!(restored.manifest, imported.manifest);
# Ok(())
# }
# enum ExampleError {
#     Import(baukfs::git_import::ImportError),
#     Restore(baukfs::restoration::RestoreError),
# }
```

## Validation and platforms

Supported targets remain x86_64-linux, aarch64-linux and aarch64-darwin. Runtime
verification must be reported separately from compilation or Nix evaluation;
Linux execution cannot verify Darwin rename semantics or filesystem policies.
Filesystem alias tests accept either two independent entries or an explicit
exclusive-creation failure with no publication. Filesystem size limits, whole-file
memory use, executor scheduling and allocator exhaustion remain caller concerns.
