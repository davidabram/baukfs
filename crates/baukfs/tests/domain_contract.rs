use baukfs::{
    IdError, IdKind, Operation, RevisionId, SessionId, TransitionError, Workspace, WorkspaceId,
    WorkspaceStatus,
};

const STATUSES: [WorkspaceStatus; 3] = [
    WorkspaceStatus::Idle,
    WorkspaceStatus::Running,
    WorkspaceStatus::Sealed,
];

const OPERATIONS: [Operation; 4] = [
    Operation::BeginRun,
    Operation::RecordQuiescence,
    Operation::RecordCheckpoint,
    Operation::Seal,
];

fn revision(value: &str) -> RevisionId {
    RevisionId::new(value).expect("valid revision id")
}

fn new_workspace() -> Workspace {
    Workspace::new(
        WorkspaceId::new("ws-1").expect("valid workspace id"),
        SessionId::new("sce:session/2026-11-10#a").expect("valid session id"),
        revision("rev-base"),
    )
}

fn workspace_with_checkpoint_in(status: WorkspaceStatus) -> Workspace {
    let mut workspace = new_workspace();
    workspace
        .record_checkpoint(revision("rev-head"))
        .expect("checkpoint while idle");
    match status {
        WorkspaceStatus::Idle => {}
        WorkspaceStatus::Running => workspace.begin_run().expect("begin while idle"),
        WorkspaceStatus::Sealed => workspace.seal().expect("seal while idle"),
    }
    assert_eq!(workspace.status(), status);
    workspace
}

fn apply(workspace: &mut Workspace, operation: Operation) -> Result<(), TransitionError> {
    match operation {
        Operation::BeginRun => workspace.begin_run(),
        Operation::RecordQuiescence => workspace.record_quiescence(),
        Operation::RecordCheckpoint => workspace.record_checkpoint(revision("rev-next")),
        Operation::Seal => workspace.seal(),
    }
}

fn expected_next(status: WorkspaceStatus, operation: Operation) -> Option<WorkspaceStatus> {
    use Operation::{BeginRun, RecordCheckpoint, RecordQuiescence, Seal};
    use WorkspaceStatus::{Idle, Running, Sealed};
    match (status, operation) {
        (Idle, BeginRun) => Some(Running),
        (Running, RecordQuiescence) | (Idle, RecordCheckpoint) => Some(Idle),
        (Idle | Sealed, Seal) => Some(Sealed),
        (Running | Sealed, BeginRun | RecordCheckpoint)
        | (Idle | Sealed, RecordQuiescence)
        | (Running, Seal) => None,
    }
}

#[test]
fn transition_table_is_enforced_for_every_status_and_operation() {
    for status in STATUSES {
        for operation in OPERATIONS {
            let before = workspace_with_checkpoint_in(status);
            let mut after = before.clone();
            let result = apply(&mut after, operation);

            if let Some(next) = expected_next(status, operation) {
                assert_eq!(result, Ok(()), "{operation} from {status}");
                assert_eq!(after.status(), next, "{operation} from {status}");
                assert_eq!(after.id(), before.id());
                assert_eq!(after.session(), before.session());
                assert_eq!(after.base(), before.base());
                let head = if operation == Operation::RecordCheckpoint {
                    "rev-next"
                } else {
                    "rev-head"
                };
                assert_eq!(after.head().as_str(), head, "{operation} from {status}");
            } else {
                assert_eq!(
                    result,
                    Err(TransitionError { operation, status }),
                    "{operation} from {status}"
                );
                assert_eq!(after, before, "{operation} from {status} mutated record");
            }
        }
    }
}

#[test]
fn new_workspace_is_idle_with_head_at_base() {
    let workspace = new_workspace();
    assert_eq!(workspace.status(), WorkspaceStatus::Idle);
    assert_eq!(workspace.id().as_str(), "ws-1");
    assert_eq!(workspace.session().as_str(), "sce:session/2026-11-10#a");
    assert_eq!(workspace.base().as_str(), "rev-base");
    assert_eq!(workspace.head(), workspace.base());
}

#[test]
fn beginning_an_already_running_workspace_is_rejected() {
    let mut workspace = new_workspace();
    workspace.begin_run().expect("begin while idle");
    let before = workspace.clone();

    assert_eq!(
        workspace.begin_run(),
        Err(TransitionError {
            operation: Operation::BeginRun,
            status: WorkspaceStatus::Running,
        })
    );
    assert_eq!(workspace, before);
}

#[test]
fn checkpoint_and_seal_are_rejected_while_running() {
    let mut workspace = new_workspace();
    workspace.begin_run().expect("begin while idle");
    let before = workspace.clone();

    assert_eq!(
        workspace.record_checkpoint(revision("rev-1")),
        Err(TransitionError {
            operation: Operation::RecordCheckpoint,
            status: WorkspaceStatus::Running,
        })
    );
    assert_eq!(workspace, before);

    assert_eq!(
        workspace.seal(),
        Err(TransitionError {
            operation: Operation::Seal,
            status: WorkspaceStatus::Running,
        })
    );
    assert_eq!(workspace, before);
}

#[test]
fn quiescence_returns_to_idle_and_allows_another_run() {
    let mut workspace = new_workspace();
    workspace.begin_run().expect("begin while idle");
    workspace
        .record_quiescence()
        .expect("quiesce while running");
    assert_eq!(workspace.status(), WorkspaceStatus::Idle);

    workspace.begin_run().expect("begin again while idle");
    assert_eq!(workspace.status(), WorkspaceStatus::Running);
}

#[test]
fn quiescence_without_a_run_is_rejected() {
    let mut workspace = new_workspace();
    let before = workspace.clone();
    assert_eq!(
        workspace.record_quiescence(),
        Err(TransitionError {
            operation: Operation::RecordQuiescence,
            status: WorkspaceStatus::Idle,
        })
    );
    assert_eq!(workspace, before);
}

#[test]
fn checkpoints_update_head_and_preserve_identity_session_and_base() {
    let mut workspace = new_workspace();
    for head in ["rev-1", "rev-2", "rev-base", "rev-2"] {
        workspace.begin_run().expect("begin while idle");
        workspace
            .record_quiescence()
            .expect("quiesce while running");
        workspace
            .record_checkpoint(revision(head))
            .expect("checkpoint while idle");

        assert_eq!(workspace.status(), WorkspaceStatus::Idle);
        assert_eq!(workspace.head().as_str(), head);
        assert_eq!(workspace.id().as_str(), "ws-1");
        assert_eq!(workspace.session().as_str(), "sce:session/2026-11-10#a");
        assert_eq!(workspace.base().as_str(), "rev-base");
    }
}

#[test]
fn sealing_finalizes_current_head_and_repeated_sealing_is_a_no_op() {
    let mut workspace = new_workspace();
    workspace
        .record_checkpoint(revision("rev-final"))
        .expect("checkpoint while idle");
    workspace.seal().expect("seal while idle");
    assert_eq!(workspace.status(), WorkspaceStatus::Sealed);
    assert_eq!(workspace.head().as_str(), "rev-final");

    let sealed = workspace.clone();
    workspace.seal().expect("repeated seal");
    workspace.seal().expect("repeated seal");
    assert_eq!(workspace, sealed);
}

#[test]
fn sealing_without_checkpoints_finalizes_the_base() {
    let mut workspace = new_workspace();
    workspace.seal().expect("seal while idle");
    assert_eq!(workspace.head(), workspace.base());
}

#[test]
fn sealed_workspace_rejects_execution_and_checkpoints() {
    let mut workspace = workspace_with_checkpoint_in(WorkspaceStatus::Sealed);
    let sealed = workspace.clone();

    for (operation, result) in [
        (Operation::BeginRun, workspace.begin_run()),
        (Operation::RecordQuiescence, workspace.record_quiescence()),
        (
            Operation::RecordCheckpoint,
            workspace.record_checkpoint(revision("rev-late")),
        ),
    ] {
        assert_eq!(
            result,
            Err(TransitionError {
                operation,
                status: WorkspaceStatus::Sealed,
            })
        );
    }
    assert_eq!(workspace, sealed);
}

#[test]
fn identifiers_preserve_accepted_values_exactly() {
    for value in [
        "a",
        " padded ",
        "UPPER-lower",
        "sce_01HZY3J6Q8",
        "not/a/path/../..",
        "550e8400-e29b-41d4-a716-446655440000",
        "naïve-日本-🦀",
    ] {
        assert_eq!(WorkspaceId::new(value).expect("valid").as_str(), value);
        assert_eq!(RevisionId::new(value).expect("valid").as_str(), value);
        assert_eq!(SessionId::new(value).expect("valid").as_str(), value);
    }
}

#[test]
fn identifiers_reject_empty_values_with_their_kind() {
    assert_eq!(
        WorkspaceId::new(""),
        Err(IdError::Empty {
            kind: IdKind::Workspace
        })
    );
    assert_eq!(
        RevisionId::new(String::new()),
        Err(IdError::Empty {
            kind: IdKind::Revision
        })
    );
    assert_eq!(
        SessionId::new(""),
        Err(IdError::Empty {
            kind: IdKind::Session
        })
    );
}

#[test]
fn identifiers_reject_control_characters_with_their_position() {
    for (value, offset, character) in [
        ("\0", 0, '\0'),
        ("a\tb", 1, '\t'),
        ("line\n", 4, '\n'),
        ("cr\r", 2, '\r'),
        ("del\u{7f}", 3, '\u{7f}'),
        ("é\u{85}", 2, '\u{85}'),
        ("ok\u{1b}[0m\n", 2, '\u{1b}'),
    ] {
        assert_eq!(
            SessionId::new(value),
            Err(IdError::ControlCharacter {
                kind: IdKind::Session,
                offset,
                character,
            }),
            "{value:?}"
        );
        assert_eq!(
            WorkspaceId::new(value).map_err(|error| error.kind()),
            Err(IdKind::Workspace)
        );
        assert_eq!(
            RevisionId::new(value).map_err(|error| error.kind()),
            Err(IdKind::Revision)
        );
    }
}
