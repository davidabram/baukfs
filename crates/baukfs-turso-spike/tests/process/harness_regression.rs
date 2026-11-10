use std::process::Command;
use std::time::{Duration, Instant};

use crate::harness::{HarnessError, Probe, Stage};
use crate::support::ok;

const SHORT_BUDGET: Duration = Duration::from_millis(500);
const CLEANUP_ALLOWANCE: Duration = Duration::from_secs(5);

fn sh(script: &str) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(script);
    command
}

fn process_exists(pid: u32) -> bool {
    sh(&format!("kill -0 {pid} 2>/dev/null"))
        .status()
        .expect("run kill -0")
        .success()
}

#[test]
fn stdout_eof_without_exit_hits_the_exit_deadline() {
    let started = Instant::now();
    let probe = ok(Probe::spawn(
        "eof-without-exit",
        sh("exec 1>&-; read line"),
        SHORT_BUDGET,
    ));
    match probe.finish() {
        Err(HarnessError::Deadline {
            stage: Stage::Exit, ..
        }) => {}
        other => panic!("expected an exit deadline, got {other:?}"),
    }
    assert!(started.elapsed() < SHORT_BUDGET + CLEANUP_ALLOWANCE);
}

#[test]
fn missing_readiness_hits_the_line_deadline() {
    let started = Instant::now();
    let mut probe = ok(Probe::spawn("never-ready", sh("read line"), SHORT_BUDGET));
    match probe.expect_line("ready") {
        Err(HarnessError::Deadline {
            stage: Stage::Line, ..
        }) => {}
        other => panic!("expected a line deadline, got {other:?}"),
    }
    assert!(!probe.is_running());
    assert!(started.elapsed() < SHORT_BUDGET + CLEANUP_ALLOWANCE);
}

#[test]
fn exit_before_readiness_reports_status_and_stderr() {
    let mut probe = ok(Probe::spawn(
        "exit-early",
        sh("echo boom >&2; exit 3"),
        Duration::from_secs(30),
    ));
    match probe.expect_line("ready") {
        Err(HarnessError::ExitedBeforeLine { status, stderr, .. }) => {
            assert_eq!(status.code(), Some(3));
            assert_eq!(stderr, "boom\n");
        }
        other => panic!("expected an early exit, got {other:?}"),
    }
}

#[test]
fn stderr_flood_before_readiness_does_not_block() {
    let script = "i=0; while [ $i -lt 4096 ]; do \
                  echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx >&2; \
                  i=$((i+1)); done; echo ready; read line";
    let mut probe = ok(Probe::spawn(
        "stderr-flood",
        sh(script),
        Duration::from_secs(30),
    ));
    ok(probe.expect_line("ready"));
    let finished = ok(probe.terminate());
    assert!(finished.lines.is_empty(), "{:?}", finished.lines);
    assert_eq!(finished.stderr.len(), 4096 * 65);
}

#[test]
fn output_after_readiness_is_drained_and_reported() {
    let mut probe = ok(Probe::spawn(
        "chatter",
        sh("printf 'ready\\nlate\\n'"),
        Duration::from_secs(30),
    ));
    ok(probe.expect_line("ready"));
    match probe.terminate_after_readiness() {
        Err(HarnessError::OutputAfterReadiness { lines, .. }) => assert_eq!(lines, ["late"]),
        other => panic!("expected trailing output to be reported, got {other:?}"),
    }
}

#[test]
fn dropped_probe_kills_and_reaps_its_child() {
    let mut probe = ok(Probe::spawn(
        "dropped",
        sh("echo ready; read line"),
        Duration::from_secs(30),
    ));
    ok(probe.expect_line("ready"));
    let pid = probe.id();
    assert!(process_exists(pid));
    drop(probe);
    assert!(!process_exists(pid), "child {pid} survived the guard");
}
