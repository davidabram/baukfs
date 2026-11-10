# Local Git commit import (T09)

`baukfs::git_import::import_commit(repository, selector, session, &mut store)` imports one explicitly selected local commit as a workspace's initial revision. The destination is an already current `WorkspaceStore`; the caller supplies the async executor. There is no runtime wrapper or CLI command.

The result is an `ImportReceipt` with `origin: GitOrigin`, `manifest: ManifestId`, and `outcome: Imported | AlreadyImported`. `GitOrigin` exposes the full `commit()`, full root `tree()`, `object_format()` (`Sha1` or `Sha256`), and matching `root_revision()`. `WorkspaceStore::read_git_origin()` reads and validates persisted provenance together with its root history from one snapshot.

## Source resolution

The repository argument is a local repository location, including a bare repository. Git must be available on the caller's trusted `PATH`. The selector is required, passed as a separate argument to `rev-parse --verify --end-of-options`, and resolved exactly once. A branch name selects its commit at resolution time. All subsequent reads use full object IDs, so later ref movement cannot change that selection. The storage object format determines whether IDs have 40 or 64 lowercase hexadecimal digits. SHA-1 and SHA-256 repositories are tested independently.

The resolved object must itself be a commit; a tree, blob, or unpeeled annotated tag returns `NonCommit`. Callers can explicitly select a tag's commit with a selector such as `tag^{commit}`. The commit object supplies the root tree ID. Invalid selectors and missing local objects have separate typed errors. Git diagnostics are retained as raw bytes. Other Git command failures preserve the operation, exit code, and bounded stderr. Missing nested objects that prevent a recursive tree listing report the selected root tree as the failed traversal's ID and retain Git's diagnostic.

Neither the working tree nor the index supplies snapshot bytes. Dirty staged/unstaged changes, untracked files, and ignore rules cannot alter the imported manifest. The operation does not move HEAD or refs, change the index, or write checkout files.

## Entries and validation

The importer enumerates with `ls-tree -r -t -z --full-tree <root-tree-id>`. It uses raw filename bytes and NUL record delimiters, never quoted or newline-delimited path output. It also reads each raw tree with `cat-file tree` and checks its original modes and component names: Git normalizes some unsupported modes in `ls-tree`, so the listing alone cannot enforce the supported-mode contract.

| Raw tree mode | Manifest entry |
| --- | --- |
| `40000` (displayed as `040000`) | Explicit directory, including an empty tree represented as a directory entry |
| `100644` | Regular file, executable false |
| `100755` | Regular file, executable true |
| `120000` | Symlink with the exact blob bytes as its target |
| `160000` | Rejected as `Gitlink`; no submodule traversal |
| Other modes | Rejected as `UnsupportedMode`, with raw mode and path bytes |

The root stays implicit. An empty root tree yields the empty manifest. Ordinary Git staging does not represent empty directories, but manually represented empty subtrees are preserved. Each regular file blob is read directly with `cat-file blob` and inserted through the existing immutable content API; existing bytes deduplicate by ContentId. Symlink blobs are never followed or read through the filesystem. Binary bytes, non-UTF-8 paths and targets, and executable flags are preserved.

Entries pass through `WorkspacePath`, `SymlinkTarget`, and `Manifest::new`. Their path, target, ordering, uniqueness, and explicit-parent requirements remain authoritative. Empty or NUL-containing symlink targets fail validation. Raw tree component names containing slash are rejected as malformed object data.

## Git execution controls

There is no shell interpolation. The child environment is cleared, retaining only the trusted executable search `PATH`. This removes inherited repository/object/index/worktree redirections, config injection variables, external-diff overrides, loader overrides, and user configuration locations. System and global configuration are disabled with `GIT_CONFIG_NOSYSTEM=1` and `GIT_CONFIG_GLOBAL=/dev/null`; system attributes are disabled. Repository configuration remains readable for repository format and local object access. Local Git directory files, linked worktrees, and object alternates retain their normal Git meaning; callers must provide a trusted repository location.

Every command overrides `core.fsmonitor=false`, `core.hooksPath=/dev/null`, `protocol.allow=never`, `maintenance.auto=false`, and `gc.auto=0`. It disables optional locks, replacement objects, terminal prompts, and paging. `GIT_NO_LAZY_FETCH=1` disables promisor lazy fetching; missing objects fail locally. The commands do not checkout, apply clean/smudge filters, request textconv or external diffs, run hooks, or recurse into submodules. Locale is fixed to C. Tests include malicious repository configuration, attributes, replacement refs, inherited Git overrides, and a missing promisor object.

Validation uses Git 2.56.0 on x86_64-linux. Deployments need Git supporting SHA-256 repositories, `--end-of-options`, `--show-object-format=storage`, and `GIT_NO_LAZY_FETCH`. The trusted Git binary and local repository/object files are not a sandbox against hostile executable replacement or concurrent filesystem tampering.

Each Git invocation has a 30-second process deadline, a 256 MiB stdout limit, and a 64 KiB stderr limit. Two concrete reader threads collect the pipes concurrently to prevent pipe-buffer deadlock. Oversized streams close at their limits; subprocess failures retain bounded diagnostics. On deadline or wait failure the child is killed and reaped; a guard also kills/reaps on early exit. These plumbing commands cannot launch configured helpers. This code is local to import and does not adopt the runner spike or implement runner supervision.

## Provenance and transactions

Schema version 4 adds:

```sql
CREATE TABLE baukfs_git_origin (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    commit_id TEXT NOT NULL,
    tree_id TEXT NOT NULL,
    object_format TEXT NOT NULL CHECK (object_format IN ('sha1', 'sha256')),
    root_revision TEXT NOT NULL
) STRICT;
```

The `baukfs_git_origin_insert_once`, `baukfs_git_origin_immutable`, and `baukfs_git_origin_undeletable` triggers prohibit duplicate insertion/replacement, updates, and deletion. Reads validate the format, exact ID spelling, revision identifier, complete history and content references, and that `root_revision` equals the workspace base. Provenance describes the imported base even after later head advancement. It records no mutable branch name or repository path; those cannot identify an immutable publication base.

The explicit 3→4 upgrade creates an empty provenance table without modifying contents, manifests, revisions, or workspace state. Upgrades from versions 1 and 2 apply all consecutive steps. Existing-only open, inspection, and create against an old schema remain non-migrating. One database still represents one workspace copy, opened with multiprocess WAL.

Content inserts use their existing independent transactions. The final import transaction takes `BEGIN IMMEDIATE`, reads and validates existing history/provenance, initializes or verifies the root through the internal history composition, inserts provenance if new, and commits both together. It does not call the public history initializer and then start a second transaction. Failures roll back new manifest, revision, workspace, and origin rows together. Previously committed unreferenced contents may remain. Trigger-injected failures after each of these four inserts verify that boundary. Commit/rollback uncertainty retains the existing discarded-connection behavior: reopen and reconcile before retrying.

## Retries and conflicts

An identical import can return `AlreadyImported` only after verifying the same session, commit, tree, format, canonical manifest and contents, and idle workspace with base and head equal to the matching root. The import still reads the selected local objects before this verification; it cannot retry successfully from a repository that has lost those objects.

A different origin returns `HistoryError::GitOriginConflict`, even when distinct commits have identical trees and consequently identical root RevisionIds. A workspace initialized without Git provenance cannot acquire provenance through a later import. Different sessions, non-idle state, or an advanced head return typed initialization conflicts. Import never resets the head or replaces origin information. Concurrent finalizations serialize through the existing write transaction.

## Resource and scope limits

Whole files, tree output, manifests, and canonical encodings are buffered in memory. The subprocess stdout limit bounds each blob or listing, not the total import size, number of entries, database memory use, or allocator failure. There is one subprocess per blob plus tree validation and enumeration calls; no batch protocol, streaming, chunking, compression, or garbage collection is introduced. History verification continues to traverse ancestry and read referenced contents.

Git calls block the calling thread despite the async storage API; callers needing executor responsiveness should schedule the import appropriately. The library owns no runtime. The per-command deadline is not a whole-import deadline, and cancellation can leave committed unreferenced content. Local object IDs keep source selection stable, but concurrent removal/corruption of objects can fail an import. No network retrieval repairs it.

Restoration, directory cloning, live filesystem capture, runner integration, branch publication, and new main-CLI commands remain deferred. T09 does not implement T10.
