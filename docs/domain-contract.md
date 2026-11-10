# Workspace domain contract

This document describes the pure, in-memory domain model in the `baukfs` crate (T02). The model does no filesystem, database, process, or async work. It records which lifecycle transitions are legal and what each one changes.

## Public types

| Type | Responsibility |
| --- | --- |
| `WorkspaceId` | Opaque identity of a workspace. |
| `RevisionId` | Opaque identity of a stored revision. |
| `SessionId` | Opaque identity of the owning session. Accepts existing SCE identifiers as-is. UUID syntax is not required. |
| `IdKind` | Which identifier kind a validation error refers to. |
| `IdError` | Why an identifier was rejected: `Empty { kind }` or `ControlCharacter { kind, offset, character }`, where `offset` is a byte offset. |
| `Workspace` | Workspace record: identity, owning session, base revision, head revision, and status. Fields are private. Changes happen only through the transition methods. |
| `WorkspaceStatus` | `Idle`, `Running`, or `Sealed`. |
| `Operation` | The attempted transition: `BeginRun`, `RecordQuiescence`, `RecordCheckpoint`, or `Seal`. |
| `TransitionError` | A rejected transition, given as `{ operation, status }`, where `status` is the status at the time of the attempt. |

### Identifiers

All three identifiers follow the same rules:

- Construction is fallible (`new(impl Into<String>)`). Empty values and values containing any Unicode control character (`char::is_control`: C0, DEL, C1) are rejected.
- Accepted values are stored exactly as given. They are not trimmed, case-folded, or Unicode-normalized.
- `as_str()` returns the stored value.
- Identifiers are metadata, not filesystem paths. Values such as `../x` are accepted and must never be used to build a path.
- The kinds are distinct types and cannot be substituted for one another.
- T02 does not provide ID generation, hashing, database key mapping, or serialization.

## Transition table

`Workspace::new(id, session, base)` creates an `Idle` workspace with `head == base`.

| Current status | `begin_run` | `record_quiescence` | `record_checkpoint(rev)` | `seal` |
| --- | --- | --- | --- | --- |
| `Idle` | → `Running` | rejected | `head = rev`, stays `Idle` | → `Sealed` (head finalized) |
| `Running` | rejected | → `Idle` | rejected | rejected |
| `Sealed` | rejected | rejected | rejected | stays `Sealed`, no change |

Every rejection returns `TransitionError { operation, status }`.

## Invariants

- The workspace id, owning session, and base revision are set at construction. No operation changes them.
- A new workspace is `Idle`, and its head equals its base.
- The head changes only through `record_checkpoint`, and only while the workspace is `Idle`.
- A rejected transition leaves the entire record unchanged (`Workspace: PartialEq`).
- `Sealed` is terminal. Once sealed, the head never changes again. Repeated `seal` calls succeed and do nothing.
- Sealing finalizes the head that is currently recorded. It does not capture the working directory.

## Deferred responsibilities

**Storage integration (Turso):**
- Confirming that a checkpoint revision exists and has been committed before `record_checkpoint` is called. The model accepts any valid `RevisionId`. It does not check that the revision exists, descends from the head, or differs from it.
- Persisting the workspace record and making transitions atomic with revision commits.
- Generating, serializing, and indexing identifiers.
- Concurrency control between multiple handles to the same workspace.

**Runner integration:**
- Deciding when quiescence has actually occurred. `record_quiescence` records an event that the future runner reports after all managed writers have stopped. A direct child process exiting is not quiescence. The model carries no token proving OS-level guarantees.
- Capturing the working directory into a revision before checkpointing or sealing.
- Process supervision, confinement, and cleanup.
