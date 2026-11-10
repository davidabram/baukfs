mod executor;
mod probe;
#[cfg(test)]
mod probe_tests;

use std::env;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use executor::block_on;
use probe::{
    ProbeError, Visibility, begin_immediate, binary_cases, commit, connect, create_schema,
    durability_settings, interrupt_visibility, open, put_content, put_interrupt_pair, read_content,
    rollback,
};

const USAGE: &str = "\
Usage: baukfs-turso-spike <COMMAND> <DATABASE>

Commands:
  roundtrip-write         Write the binary round-trip cases and exit
  roundtrip-verify        Reopen and compare every binary round-trip case
  transaction-commit      Write content and head in one transaction, commit, print visibility
  transaction-rollback    Write content and head in one transaction, roll back, print visibility
  hold-uncommitted        Write content and head in a transaction, report ready, wait for termination
  hold-committed          Commit content and head, report ready, wait for termination
  inspect                 Print which interrupt records are visible
  settings                Print journal_mode and synchronous
";

enum Command {
    RoundtripWrite,
    RoundtripVerify,
    TransactionCommit,
    TransactionRollback,
    HoldUncommitted,
    HoldCommitted,
    Inspect,
    Settings,
}

enum Failure {
    Usage(String),
    Probe(ProbeError),
    Mismatch(String),
    Io(io::Error),
}

impl From<ProbeError> for Failure {
    fn from(error: ProbeError) -> Self {
        Self::Probe(error)
    }
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn parse(mut args: impl Iterator<Item = OsString>) -> Result<(Command, PathBuf), Failure> {
    let command = match args.next() {
        Some(arg) if arg == "roundtrip-write" => Command::RoundtripWrite,
        Some(arg) if arg == "roundtrip-verify" => Command::RoundtripVerify,
        Some(arg) if arg == "transaction-commit" => Command::TransactionCommit,
        Some(arg) if arg == "transaction-rollback" => Command::TransactionRollback,
        Some(arg) if arg == "hold-uncommitted" => Command::HoldUncommitted,
        Some(arg) if arg == "hold-committed" => Command::HoldCommitted,
        Some(arg) if arg == "inspect" => Command::Inspect,
        Some(arg) if arg == "settings" => Command::Settings,
        Some(arg) => {
            return Err(Failure::Usage(format!(
                "unknown command '{}'",
                arg.to_string_lossy()
            )));
        }
        None => return Err(Failure::Usage("missing command".to_owned())),
    };
    let Some(database) = args.next() else {
        return Err(Failure::Usage("missing database path".to_owned()));
    };
    if let Some(extra) = args.next() {
        return Err(Failure::Usage(format!(
            "unexpected argument '{}'",
            extra.to_string_lossy()
        )));
    }
    Ok((command, PathBuf::from(database)))
}

fn report(line: &str) -> Result<(), Failure> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{line}")?;
    stdout.flush()?;
    Ok(())
}

fn report_visibility(visibility: Visibility) -> Result<(), Failure> {
    report(&format!(
        "content={} head={}",
        presence(visibility.content),
        presence(visibility.head)
    ))
}

fn presence(visible: bool) -> &'static str {
    if visible { "present" } else { "absent" }
}

fn wait_for_parent_termination() -> Result<(), Failure> {
    let mut sink = Vec::new();
    io::stdin().lock().read_to_end(&mut sink)?;
    Err(Failure::Mismatch(
        "stdin closed before the parent terminated the probe".to_owned(),
    ))
}

async fn run(command: Command, path: PathBuf) -> Result<(), Failure> {
    let database = open(&path).await?;
    let mut connection = connect(&database)?;
    match command {
        Command::RoundtripWrite => {
            create_schema(&connection).await?;
            let cases = binary_cases();
            for (key, bytes) in &cases {
                put_content(&connection, key, bytes).await?;
            }
            report(&format!("wrote {} cases", cases.len()))
        }
        Command::RoundtripVerify => {
            let cases = binary_cases();
            for (key, expected) in &cases {
                let Some(stored) = read_content(&connection, key).await? else {
                    return Err(Failure::Mismatch(format!("{key}: missing after reopen")));
                };
                let expected_length = i64::try_from(expected.len()).unwrap_or(i64::MAX);
                if stored.bytes != *expected
                    || stored.sql_type != "blob"
                    || stored.sql_length != expected_length
                {
                    return Err(Failure::Mismatch(format!(
                        "{key}: got {} bytes typeof={} length()={}, want {} bytes typeof=blob",
                        stored.bytes.len(),
                        stored.sql_type,
                        stored.sql_length,
                        expected.len()
                    )));
                }
            }
            report(&format!("verified {} cases", cases.len()))
        }
        Command::TransactionCommit => {
            create_schema(&connection).await?;
            let transaction = begin_immediate(&mut connection).await?;
            put_interrupt_pair(&transaction).await?;
            commit(transaction).await?;
            report_visibility(interrupt_visibility(&connection).await?)
        }
        Command::TransactionRollback => {
            create_schema(&connection).await?;
            let transaction = begin_immediate(&mut connection).await?;
            put_interrupt_pair(&transaction).await?;
            rollback(transaction).await?;
            report_visibility(interrupt_visibility(&connection).await?)
        }
        Command::HoldUncommitted => {
            create_schema(&connection).await?;
            let transaction = begin_immediate(&mut connection).await?;
            put_interrupt_pair(&transaction).await?;
            report("ready: uncommitted")?;
            wait_for_parent_termination()
        }
        Command::HoldCommitted => {
            create_schema(&connection).await?;
            let transaction = begin_immediate(&mut connection).await?;
            put_interrupt_pair(&transaction).await?;
            commit(transaction).await?;
            report("ready: committed")?;
            wait_for_parent_termination()
        }
        Command::Inspect => report_visibility(interrupt_visibility(&connection).await?),
        Command::Settings => {
            let settings = durability_settings(&connection).await?;
            report(&format!(
                "journal_mode={} synchronous={}",
                settings.journal_mode, settings.synchronous
            ))
        }
    }
}

fn main() -> ExitCode {
    let outcome =
        parse(env::args_os().skip(1)).and_then(|(command, path)| block_on(run(command, path)));
    let mut stderr = io::stderr().lock();
    let written = match outcome {
        Ok(()) => return ExitCode::SUCCESS,
        Err(Failure::Usage(message)) => {
            let _ = write!(stderr, "error: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
        Err(Failure::Probe(error)) => writeln!(stderr, "error: {error}\ndetail: {error:?}"),
        Err(Failure::Mismatch(message)) => writeln!(stderr, "error: {message}"),
        Err(Failure::Io(error)) => writeln!(stderr, "error: probe I/O: {error}"),
    };
    drop(written);
    ExitCode::FAILURE
}
