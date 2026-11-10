mod support;

use std::fs;
use std::time::Instant;

use support::{
    Controller, Fixture, Fixtures, REPLY_TIMEOUT, Scratch, assert_starts_with, controller, field,
};

const LIVE_BASIS: &str = if cfg!(target_os = "linux") {
    "admit granted basis=subreaper-empty"
} else {
    "admit granted basis=group-and-token-released"
};
const FRESH: &str = "recovery=no-workload(no-interrupted-run)";

fn start_run(scratch: &Scratch, fixtures: &mut Fixtures) -> (Controller, Fixture) {
    let mut ctl = controller(scratch, "ws");
    assert_starts_with(&ctl.ready, "ready");
    assert_eq!(
        field(&ctl.ready, "recovery"),
        FRESH.strip_prefix("recovery=")
    );
    let spawned = ctl.send("spawn root");
    assert_starts_with(&spawned, "spawned");
    let root = fixtures.expect("root");
    assert_eq!(field(&spawned, "pid"), Some(root.pid.to_string().as_str()));
    assert_eq!(root.pgid, root.pid, "root leads its own process group");
    (ctl, root)
}

fn spawn_child(parent: &mut Fixture, fixtures: &mut Fixtures, role: &str) -> Fixture {
    let reply = parent.send(&format!("spawn {role}"));
    assert_starts_with(&reply, "spawned");
    let child = fixtures.expect(role);
    assert_eq!(child.ppid, parent.pid);
    assert_eq!(
        child.pgid, parent.pgid,
        "children start in the parent's group"
    );
    assert_eq!(
        child.sid, parent.sid,
        "children start in the parent's session"
    );
    child
}

fn assert_refused_active(ctl: &mut Controller, expected: &[&str]) -> String {
    let reply = ctl.send("admit");
    assert_starts_with(&reply, "admit refused active");
    for indicator in expected {
        assert!(
            reply
                .split_ascii_whitespace()
                .any(|word| word == *indicator),
            "{reply:?} lacks {indicator:?}"
        );
    }
    reply
}

fn children(pids: &[i32]) -> String {
    let list: Vec<String> = pids.iter().map(ToString::to_string).collect();
    format!("children[{}]", list.join(","))
}

fn pid_set(list: Option<&str>) -> Vec<i32> {
    let list = list.expect("pid list field");
    let inner = list.trim_start_matches('[').trim_end_matches(']');
    let mut pids: Vec<i32> = inner
        .split(',')
        .filter(|pid| !pid.is_empty())
        .map(|pid| pid.parse().expect("pid"))
        .collect();
    pids.sort_unstable();
    pids
}

/// Socket end-of-file precedes the kernel's deferred release of the exiting
/// fixture's other files, so recovery re-checks OS state within a bound.
fn reclassify_until(ctl: &mut Controller, expected: &str) {
    let deadline = Instant::now() + REPLY_TIMEOUT;
    loop {
        let reply = ctl.send("reclassify");
        if reply == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "expected {expected:?}, last {reply:?}"
        );
        std::thread::yield_now();
    }
}

fn settle_and_admit(ctl: &mut Controller) {
    assert_starts_with(&ctl.send("settle 10000"), "settled");
    assert_eq!(ctl.send("admit"), LIVE_BASIS);
}

#[test]
fn normal_exit_is_admitted_after_root_is_reaped() {
    let scratch = Scratch::new();
    let mut fixtures = Fixtures::listen(&scratch);
    let (mut ctl, mut root) = start_run(&scratch, &mut fixtures);

    assert_eq!(root.send("write out"), "wrote out");
    assert_refused_active(&mut ctl, &["root-running", "group-alive", "token-held"]);
    root.exit(0);
    assert_eq!(ctl.send("wait-root 10000"), "root-exited code=0");
    assert_eq!(ctl.send("admit"), LIVE_BASIS);
    assert_eq!(
        scratch.read_work_file("out"),
        format!("root {}\n", root.pid)
    );
}

#[test]
fn failing_command_is_admitted_once_quiescent() {
    let scratch = Scratch::new();
    let mut fixtures = Fixtures::listen(&scratch);
    let (mut ctl, mut root) = start_run(&scratch, &mut fixtures);

    root.exit(3);
    assert_eq!(ctl.send("wait-root 10000"), "root-exited code=3");
    assert_eq!(ctl.send("admit"), LIVE_BASIS);
}

#[test]
fn child_writing_after_parent_exit_blocks_admission() {
    let scratch = Scratch::new();
    let mut fixtures = Fixtures::listen(&scratch);
    let (mut ctl, mut root) = start_run(&scratch, &mut fixtures);
    let mut child = spawn_child(&mut root, &mut fixtures, "child");

    root.exit(0);
    assert_eq!(ctl.send("wait-root 10000"), "root-exited code=0");
    let mut expected = Vec::from(["group-alive", "token-held"]);
    let tracked = children(&[child.pid]);
    if cfg!(target_os = "linux") {
        expected.push(&tracked);
    }
    assert_refused_active(&mut ctl, &expected);

    assert_eq!(child.send("write out"), "wrote out");
    assert_refused_active(&mut ctl, &expected);
    child.exit(0);
    settle_and_admit(&mut ctl);
    assert_eq!(
        scratch.read_work_file("out"),
        format!("child {}\n", child.pid)
    );
}

#[test]
fn grandchild_outliving_ancestors_blocks_admission() {
    let scratch = Scratch::new();
    let mut fixtures = Fixtures::listen(&scratch);
    let (mut ctl, mut root) = start_run(&scratch, &mut fixtures);
    let mut child = spawn_child(&mut root, &mut fixtures, "child");
    let mut grandchild = spawn_child(&mut child, &mut fixtures, "grandchild");

    child.exit(0);
    root.exit(0);
    assert_eq!(ctl.send("wait-root 10000"), "root-exited code=0");
    if cfg!(target_os = "linux") {
        let deadline = Instant::now() + REPLY_TIMEOUT;
        while parent_of(grandchild.pid) != Some(ctl.pid()) {
            assert!(
                Instant::now() < deadline,
                "the orphaned grandchild is reparented to the subreaper controller"
            );
            std::thread::yield_now();
        }
        let observation = ctl.send("settle 0");
        assert_eq!(
            pid_set(field(&observation, "descendants")),
            [grandchild.pid],
            "{observation}"
        );
    }
    assert_refused_active(&mut ctl, &["group-alive", "token-held"]);

    assert_eq!(grandchild.send("write out"), "wrote out");
    grandchild.exit(0);
    settle_and_admit(&mut ctl);
}

#[test]
fn descendant_with_closed_stdio_keeps_writing_and_blocks_admission() {
    let scratch = Scratch::new();
    let mut fixtures = Fixtures::listen(&scratch);
    let (mut ctl, mut root) = start_run(&scratch, &mut fixtures);
    let mut child = spawn_child(&mut root, &mut fixtures, "child");

    assert_eq!(child.send("close-stdio"), "closed-stdio");
    root.exit(0);
    assert_eq!(ctl.send("wait-root 10000"), "root-exited code=0");
    let deadline = Instant::now() + REPLY_TIMEOUT;
    let diagnostics = loop {
        let diagnostics = ctl.send("diagnostics");
        if field(&diagnostics, "stdout-open") == Some("no")
            && field(&diagnostics, "stderr-open") == Some("no")
        {
            break diagnostics;
        }
        assert!(
            Instant::now() < deadline,
            "pipes stayed open: {diagnostics}"
        );
        std::thread::yield_now();
    };
    assert_ne!(field(&diagnostics, "stdout-bytes"), Some("0"));
    assert_ne!(field(&diagnostics, "stderr-bytes"), Some("0"));

    assert_refused_active(&mut ctl, &["group-alive", "token-held"]);
    assert_eq!(child.send("write out"), "wrote out");
    child.exit(0);
    settle_and_admit(&mut ctl);
}

#[test]
fn detached_descendant_escapes_process_group_and_token() {
    let scratch = Scratch::new();
    let mut fixtures = Fixtures::listen(&scratch);
    let (mut ctl, mut root) = start_run(&scratch, &mut fixtures);
    let mut child = spawn_child(&mut root, &mut fixtures, "child");

    let detached = child.send("setsid");
    assert_starts_with(&detached, "detached");
    assert_eq!(
        field(&detached, "sid"),
        Some(child.pid.to_string().as_str())
    );
    assert_eq!(
        field(&detached, "pgid"),
        Some(child.pid.to_string().as_str())
    );
    root.exit(0);
    assert_eq!(ctl.send("wait-root 10000"), "root-exited code=0");

    let observation = ctl.send("observe");
    assert_eq!(
        field(&observation, "group"),
        Some("empty"),
        "a new session leaves the process group: {observation}"
    );
    assert_eq!(field(&observation, "token"), Some("held"));
    let tracked = children(&[child.pid]);
    if cfg!(target_os = "linux") {
        assert_refused_active(&mut ctl, &[&tracked, "token-held"]);
    } else {
        assert_refused_active(&mut ctl, &["token-held"]);
    }

    assert_eq!(child.send("close-fds"), "closed-fds");
    let observation = ctl.send("observe");
    assert_eq!(field(&observation, "group"), Some("empty"));
    assert_eq!(
        field(&observation, "token"),
        Some("free"),
        "closing inherited descriptors escapes the token: {observation}"
    );
    if cfg!(target_os = "linux") {
        assert_eq!(
            assert_refused_active(&mut ctl, &[&tracked]),
            format!("admit refused active {tracked}")
        );
    } else {
        // Negative result: the narrower macOS contract admits while this writer lives.
        assert_eq!(ctl.send("admit"), LIVE_BASIS);
    }
    assert_eq!(child.send("write out"), "wrote out");
    child.exit(0);
    settle_and_admit(&mut ctl);
}

#[test]
fn termination_escalates_for_descendant_ignoring_sigterm() {
    let scratch = Scratch::new();
    let mut fixtures = Fixtures::listen(&scratch);
    let (mut ctl, mut root) = start_run(&scratch, &mut fixtures);
    let mut stubborn = spawn_child(&mut root, &mut fixtures, "stubborn");
    assert_eq!(stubborn.send("ignore-term"), "ignoring-term");

    let report = ctl.send("terminate 300");
    assert_starts_with(&report, "terminated escalated=yes quiescent=yes");
    assert_eq!(field(&report, "group-term"), Some("delivered"));
    if cfg!(target_os = "linux") {
        // The group signal already reached the root; individual signals
        // cover descendants that are still alive.
        assert_eq!(pid_set(field(&report, "term")), [stubborn.pid]);
        assert_eq!(
            field(&report, "kill"),
            Some(format!("[{}]", stubborn.pid).as_str())
        );
    }
    root.wait_closed();
    stubborn.wait_closed();
    assert_eq!(ctl.send("wait-root 0"), "root-exited signal=15");
    assert_eq!(ctl.send("admit"), LIVE_BASIS);
}

#[test]
fn termination_reach_for_detached_descendant() {
    let scratch = Scratch::new();
    let mut fixtures = Fixtures::listen(&scratch);
    let (mut ctl, mut root) = start_run(&scratch, &mut fixtures);
    let mut child = spawn_child(&mut root, &mut fixtures, "child");
    assert_starts_with(&child.send("setsid"), "detached");

    let report = ctl.send("terminate 2000");
    root.wait_closed();
    assert_eq!(ctl.send("wait-root 0"), "root-exited signal=15");
    if cfg!(target_os = "linux") {
        assert_starts_with(&report, "terminated escalated=no quiescent=yes");
        assert_eq!(pid_set(field(&report, "term")), [child.pid]);
        child.wait_closed();
        assert_eq!(ctl.send("admit"), LIVE_BASIS);
    } else {
        // Process-group signals cannot reach a descendant in another session.
        assert_starts_with(&report, "terminated escalated=yes quiescent=no");
        assert_eq!(field(&report, "token"), Some("held"));
        assert_refused_active(&mut ctl, &["token-held"]);
        child.exit(0);
        settle_and_admit(&mut ctl);
    }
}

#[test]
fn controller_ownership_is_exclusive_per_workspace_and_released_on_death() {
    let scratch = Scratch::new();
    let mut first = controller(&scratch, "one");
    assert_starts_with(&first.ready, "ready");

    let mut rival = controller(&scratch, "one");
    assert_eq!(rival.ready, "error ownership-held");
    assert_eq!(rival.wait_exit().code(), Some(3));

    let other = controller(&scratch, "two");
    assert_starts_with(&other.ready, "ready");

    assert!(!first.kill().success());
    let successor = controller(&scratch, "one");
    assert_starts_with(&successor.ready, "ready");
}

#[test]
fn replaced_lock_file_revokes_ownership_for_admission() {
    let scratch = Scratch::new();
    let mut first = controller(&scratch, "ws");
    assert_starts_with(&first.ready, "ready");
    fs::remove_file(scratch.state("ws").join("controller.lock")).expect("unlink lock");

    let mut usurper = controller(&scratch, "ws");
    assert_starts_with(&usurper.ready, "ready");
    assert_starts_with(&first.send("admit"), "admit refused ownership");
    assert_eq!(
        usurper.send("admit"),
        "admit granted basis=no-interrupted-run"
    );
}

#[test]
fn clean_prior_run_is_classified_as_no_workload() {
    let scratch = Scratch::new();
    let mut fixtures = Fixtures::listen(&scratch);
    let (mut first, mut root) = start_run(&scratch, &mut fixtures);
    root.exit(0);
    assert_eq!(first.send("wait-root 10000"), "root-exited code=0");
    assert_eq!(first.send("admit"), LIVE_BASIS);
    assert!(first.wait_exit().success());

    let mut next = controller(&scratch, "ws");
    assert_eq!(
        field(&next.ready, "recovery"),
        FRESH.strip_prefix("recovery=")
    );
    assert_eq!(next.send("admit"), "admit granted basis=no-interrupted-run");
}

#[test]
fn controller_death_leaves_live_abandoned_workload_then_unresolved() {
    let scratch = Scratch::new();
    let mut fixtures = Fixtures::listen(&scratch);
    let (mut first, mut root) = start_run(&scratch, &mut fixtures);
    let mut child = spawn_child(&mut root, &mut fixtures, "child");

    first.kill();
    assert_eq!(
        child.send("write out"),
        "wrote out",
        "writer outlives its controller"
    );

    let mut next = controller(&scratch, "ws");
    assert_eq!(
        field(&next.ready, "recovery"),
        Some("live-abandoned(token-held)")
    );
    assert_eq!(next.send("admit"), "admit refused abandoned token-held");

    child.exit(0);
    root.exit(0);
    reclassify_until(&mut next, "recovery unresolved(interrupted-run-token-free)");
    assert_eq!(
        next.send("admit"),
        "admit refused unknown interrupted-run-token-free"
    );
}

#[test]
fn controller_death_with_fd_closing_writer_is_unresolved_not_quiescent() {
    let scratch = Scratch::new();
    let mut fixtures = Fixtures::listen(&scratch);
    let (mut first, mut root) = start_run(&scratch, &mut fixtures);
    let mut child = spawn_child(&mut root, &mut fixtures, "child");
    assert_eq!(child.send("close-fds"), "closed-fds");
    root.exit(0);
    assert_eq!(first.send("wait-root 10000"), "root-exited code=0");
    assert_refused_active(&mut first, &["group-alive"]);

    first.kill();
    let mut next = controller(&scratch, "ws");
    assert_eq!(
        field(&next.ready, "recovery"),
        Some("unresolved(interrupted-run-token-free)")
    );
    assert_eq!(
        child.send("write out"),
        "wrote out",
        "the unresolved writer is live"
    );
    assert_eq!(
        next.send("admit"),
        "admit refused unknown interrupted-run-token-free"
    );
    child.exit(0);
}

#[cfg(target_os = "linux")]
fn parent_of(pid: i32) -> Option<i32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_ascii_whitespace().nth(1)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
fn parent_of(_pid: i32) -> Option<i32> {
    None
}

#[cfg(target_os = "linux")]
mod cgroup {
    use super::support::ControllerArgs;
    use super::*;

    fn scoped_controller(scratch: &Scratch) -> Controller {
        let ctl = Controller::start(&ControllerArgs {
            scratch,
            workspace: "ws",
            cgroup: true,
            systemd_scope: true,
        });
        assert_starts_with(&ctl.ready, "ready");
        ctl
    }

    #[test]
    #[ignore = "requires a systemd user manager with cgroup v2 delegation"]
    fn cgroup_tracks_escaped_writer_across_controller_death() {
        let scratch = Scratch::new();
        let mut fixtures = Fixtures::listen(&scratch);
        let mut first = scoped_controller(&scratch);
        assert_starts_with(&first.send("spawn root"), "spawned");
        let mut root = fixtures.expect("root");
        let mut child = spawn_child(&mut root, &mut fixtures, "child");
        assert_starts_with(&child.send("setsid"), "detached");
        assert_eq!(child.send("close-fds"), "closed-fds");
        root.exit(0);
        assert_eq!(first.send("wait-root 10000"), "root-exited code=0");
        let observation = first.send("observe");
        assert_eq!(field(&observation, "group"), Some("empty"));
        assert_eq!(field(&observation, "token"), Some("free"));
        assert_eq!(field(&observation, "cgroup"), Some("populated"));

        first.kill();
        let mut next = controller(&scratch, "ws");
        assert_eq!(
            field(&next.ready, "recovery"),
            Some("live-abandoned(cgroup-populated)")
        );
        assert_eq!(child.send("write out"), "wrote out");
        assert_eq!(
            next.send("admit"),
            "admit refused abandoned cgroup-populated"
        );

        child.exit(0);
        reclassify_until(&mut next, "recovery no-workload(cgroup-empty)");
        assert_eq!(next.send("admit"), "admit granted basis=cgroup-empty");
    }

    #[test]
    #[ignore = "requires a systemd user manager with cgroup v2 delegation"]
    fn cgroup_kill_reaches_escaped_descendant_ignoring_sigterm() {
        let scratch = Scratch::new();
        let mut fixtures = Fixtures::listen(&scratch);
        let mut ctl = scoped_controller(&scratch);
        assert_starts_with(&ctl.send("spawn root"), "spawned");
        let mut root = fixtures.expect("root");
        let mut child = spawn_child(&mut root, &mut fixtures, "child");
        assert_starts_with(&child.send("setsid"), "detached");
        assert_eq!(child.send("close-fds"), "closed-fds");
        assert_eq!(child.send("ignore-term"), "ignoring-term");

        let report = ctl.send("terminate 300");
        assert_starts_with(&report, "terminated escalated=yes quiescent=yes");
        assert_eq!(field(&report, "cgroup"), Some("empty"));
        root.wait_closed();
        child.wait_closed();
        assert_eq!(ctl.send("admit"), "admit granted basis=cgroup-empty");
    }
}
