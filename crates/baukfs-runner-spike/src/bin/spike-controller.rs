//! Line-oriented controller used by the spike's integration tests.
//!
//! Prints `ready ...` after acquiring ownership, then answers one stdin
//! command per line with one stdout line. Exits 3 if another controller owns
//! the workspace.

use std::env;
use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::Duration;

use baukfs_runner_spike::admission::{
    self, ActiveWorkload, AdmissionRefusal, UnknownReason, WorkloadState,
};
use baukfs_runner_spike::ownership::{ControllerLock, OwnershipError, WorkspaceState};
use baukfs_runner_spike::process::Delivery;
use baukfs_runner_spike::record;
use baukfs_runner_spike::recovery::{self, RecoveryState};
use baukfs_runner_spike::supervisor::{self, PidList, Supervisor, SupervisorOptions};

const USAGE: &str =
    "usage: spike-controller --state <dir> --workdir <dir> --fixture <path> [--cgroup]";

struct Config {
    state: PathBuf,
    workdir: PathBuf,
    fixture: PathBuf,
    options: SupervisorOptions,
}

fn parse(mut args: impl Iterator<Item = OsString>) -> Option<Config> {
    let (mut state, mut workdir, mut fixture) = (None, None, None);
    let mut options = SupervisorOptions::default();
    while let Some(arg) = args.next() {
        match arg.to_str()? {
            "--state" => state = Some(PathBuf::from(args.next()?)),
            "--workdir" => workdir = Some(PathBuf::from(args.next()?)),
            "--fixture" => fixture = Some(PathBuf::from(args.next()?)),
            "--cgroup" => options.cgroup = true,
            _ => return None,
        }
    }
    Some(Config {
        state: state?,
        workdir: workdir?,
        fixture: fixture?,
        options,
    })
}

struct Controller {
    config: Config,
    lock: ControllerLock,
    prior: RecoveryState,
    run: Option<Supervisor>,
}

fn millis(argument: Option<&str>) -> Option<Duration> {
    argument?.parse().ok().map(Duration::from_millis)
}

impl Controller {
    fn workload_state(&mut self) -> WorkloadState {
        match &self.prior {
            RecoveryState::LiveAbandoned(evidence) => {
                return WorkloadState::Active(ActiveWorkload::Abandoned(*evidence));
            }
            RecoveryState::Unresolved(reason) => {
                return WorkloadState::Unknown(UnknownReason::PriorRunUnresolved(*reason));
            }
            RecoveryState::NoWorkload(proof) => {
                if self.run.is_none() {
                    return WorkloadState::Quiescent(proof.clone());
                }
            }
        }
        let Some(run) = &mut self.run else {
            unreachable!("handled above");
        };
        match run.observe() {
            Ok(observation) => run.assess(&observation),
            Err(error) => WorkloadState::Unknown(UnknownReason::Probe(error)),
        }
    }

    fn handle(&mut self, line: &str) -> Option<String> {
        let mut words = line.split_ascii_whitespace();
        let command = words.next()?;
        let reply = match command {
            "spawn" => self.spawn(words.collect()),
            "wait-root" => match (&mut self.run, millis(words.next())) {
                (Some(run), Some(timeout)) => match run.wait_root(timeout) {
                    Ok(Some(status)) => format!("root-exited {}", describe(status)),
                    Ok(None) => "root-running".to_owned(),
                    Err(error) => format!("error {error}"),
                },
                (None, _) => "error no-run".to_owned(),
                (_, None) => "error usage".to_owned(),
            },
            "observe" => match &mut self.run {
                Some(run) => match run.observe() {
                    Ok(observation) => format!("observation {observation}"),
                    Err(error) => format!("error {error}"),
                },
                None => "error no-run".to_owned(),
            },
            "settle" => match (&mut self.run, millis(words.next())) {
                (Some(run), Some(timeout)) => match run.settle(timeout) {
                    Ok((true, observation)) => format!("settled {observation}"),
                    Ok((false, observation)) => format!("unsettled {observation}"),
                    Err(error) => format!("error {error}"),
                },
                (None, _) => "error no-run".to_owned(),
                (_, None) => "error usage".to_owned(),
            },
            "admit" => self.admit(),
            "terminate" => match (&mut self.run, millis(words.next())) {
                (Some(run), Some(grace)) => match run.terminate(grace, Duration::from_secs(5)) {
                    Ok(report) => format!(
                        "terminated escalated={} quiescent={} group-term={} term={} kill={} {}",
                        yes_no(report.escalated),
                        yes_no(report.quiescent),
                        match report.group_terminate {
                            Delivery::Delivered => "delivered",
                            Delivery::NoSuchProcess => "no-such-group",
                        },
                        PidList(&report.terminated),
                        PidList(&report.killed),
                        report.last
                    ),
                    Err(error) => format!("error {error}"),
                },
                (None, _) => "error no-run".to_owned(),
                (_, None) => "error usage".to_owned(),
            },
            "diagnostics" => match &self.run {
                Some(run) => format!(
                    "diagnostics stdout-open={} stderr-open={} stdout-bytes={} stderr-bytes={}",
                    yes_no(run.stdout().is_open()),
                    yes_no(run.stderr().is_open()),
                    run.stdout().captured().len(),
                    run.stderr().captured().len()
                ),
                None => "error no-run".to_owned(),
            },
            "reclassify" => match recovery::classify(&self.lock) {
                Ok(prior) => {
                    let reply = format!("recovery {prior}");
                    self.prior = prior;
                    reply
                }
                Err(error) => format!("error {error}"),
            },
            "quit" => return None,
            _ => format!("error unknown-command {command}"),
        };
        Some(reply)
    }

    fn spawn(&mut self, args: Vec<&str>) -> String {
        if self.run.is_some() {
            return "error already-running".to_owned();
        }
        let mut command = Command::new(&self.config.fixture);
        command
            .args(args)
            .current_dir(&self.config.workdir)
            .env("BAUKFS_SPIKE_WORKDIR", &self.config.workdir);
        match Supervisor::spawn(&self.lock, command, self.config.options) {
            Ok(run) => {
                let reply = format!("spawned pid={} pgid={}", run.root(), run.root());
                self.run = Some(run);
                reply
            }
            Err(error) => format!("error {error}"),
        }
    }

    fn admit(&mut self) -> String {
        let state = self.workload_state();
        match admission::admit(&self.lock, state) {
            Ok(admission) => {
                if self.run.is_some()
                    && let Err(error) = record::write_quiescent(self.lock.state())
                {
                    return format!("error {error}");
                }
                format!("admit granted basis={}", admission.basis)
            }
            Err(AdmissionRefusal::Ownership(error)) => {
                format!("admit refused ownership {error}")
            }
            Err(AdmissionRefusal::ForeignWorkspace) => "admit refused foreign-workspace".to_owned(),
            Err(AdmissionRefusal::Active(ActiveWorkload::Supervised(indicators))) => {
                let mut reply = "admit refused active".to_owned();
                for indicator in indicators {
                    reply.push(' ');
                    reply.push_str(&indicator.to_string());
                }
                reply
            }
            Err(AdmissionRefusal::Active(ActiveWorkload::Abandoned(evidence))) => {
                format!("admit refused abandoned {evidence}")
            }
            Err(AdmissionRefusal::Unknown(UnknownReason::PriorRunUnresolved(reason))) => {
                format!("admit refused unknown {reason}")
            }
            Err(AdmissionRefusal::Unknown(UnknownReason::Probe(error))) => {
                format!("admit refused unknown probe {error}")
            }
        }
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn describe(status: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("code={code}"),
        (None, Some(signal)) => format!("signal={signal}"),
        (None, None) => format!("raw={}", status.into_raw()),
    }
}

fn main() -> ExitCode {
    let Some(config) = parse(env::args_os().skip(1)) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let mut stdout = io::stdout().lock();
    let state = match WorkspaceState::open(&config.state) {
        Ok(state) => state,
        Err(error) => {
            let _ = writeln!(stdout, "error {error}");
            return ExitCode::from(2);
        }
    };
    let lock = match ControllerLock::try_acquire(state) {
        Ok(lock) => lock,
        Err(OwnershipError::Held { .. }) => {
            let _ = writeln!(stdout, "error ownership-held");
            return ExitCode::from(3);
        }
        Err(error) => {
            let _ = writeln!(stdout, "error {error}");
            return ExitCode::from(2);
        }
    };
    if let Err(error) = supervisor::prepare_controller() {
        let _ = writeln!(stdout, "error cannot prepare controller: {error}");
        return ExitCode::from(2);
    }
    let prior = match recovery::classify(&lock) {
        Ok(prior) => prior,
        Err(error) => {
            let _ = writeln!(stdout, "error {error}");
            return ExitCode::from(2);
        }
    };
    if writeln!(stdout, "ready pid={} recovery={prior}", std::process::id()).is_err() {
        return ExitCode::FAILURE;
    }
    let mut controller = Controller {
        config,
        lock,
        prior,
        run: None,
    };
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else {
            return ExitCode::FAILURE;
        };
        let Some(reply) = controller.handle(&line) else {
            break;
        };
        if writeln!(stdout, "{reply}").is_err() {
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
