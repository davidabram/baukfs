mod executor;
mod probe;
#[cfg(test)]
mod probe_tests;

use std::env;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use turso::Connection;
use turso::transaction::Transaction;

use executor::block_on;
use probe::{
    ExistingOpen, OpenMode, ProbeConfig, ProbeError, Readiness, Visibility, begin_immediate,
    begin_immediate_unchecked, binary_cases, commit, connect, durability_settings,
    ensure_initialized, initialize_under_lock, interrupt_visibility, open, open_existing,
    put_content, put_head, put_interrupt_pair, read_content, read_head, readiness, rollback,
    schema_cookie,
};

const USAGE: &str = "\
Usage: baukfs-turso-spike <COMMAND> --mode <default|multiprocess> --busy-timeout-ms <MS> <DATABASE>

Every command prints `config mode=<MODE> busy_timeout_ms=<MS>` as its first line.
A busy timeout of 0 leaves the connection's busy handler unset.

Commands:
  roundtrip-write         Write the binary round-trip cases and exit
  roundtrip-verify        Reopen and compare every binary round-trip case
  transaction-commit      Write content and head in one transaction, commit, print visibility
  transaction-rollback    Write content and head in one transaction, roll back, print visibility
  inspect                 Print which interrupt records are visible
  settings                Print journal_mode and synchronous
  initialize              Print the read-only readiness precheck, then initialize under BEGIN IMMEDIATE
  inspect-existing        Open read-only without creating; print readiness, schema cookie, and visibility
  session                 Answer line commands on stdin until `quit`

Session commands (one response line each):
  begin | commit | rollback
  put <KEY> <HEX>         Insert content
  head <NAME> <KEY>       Insert a head
  get <KEY> <NAME>        Print content and head
  write-pair <KEY> <NAME> <HEX>
                          Print `started`, then begin, insert content and head, and commit
  init-hold               Begin, recheck readiness, apply the schema, keep the transaction open
  quit
";

enum Command {
    RoundtripWrite,
    RoundtripVerify,
    TransactionCommit,
    TransactionRollback,
    Inspect,
    Settings,
    Initialize,
    InspectExisting,
    Session,
}

impl Command {
    fn parse(value: &OsString) -> Option<Self> {
        Some(match value.to_str()? {
            "roundtrip-write" => Self::RoundtripWrite,
            "roundtrip-verify" => Self::RoundtripVerify,
            "transaction-commit" => Self::TransactionCommit,
            "transaction-rollback" => Self::TransactionRollback,
            "inspect" => Self::Inspect,
            "settings" => Self::Settings,
            "initialize" => Self::Initialize,
            "inspect-existing" => Self::InspectExisting,
            "session" => Self::Session,
            _ => return None,
        })
    }
}

struct Invocation {
    command: Command,
    config: ProbeConfig,
    database: PathBuf,
}

enum Failure {
    Usage(String),
    Probe(ProbeError),
    Mismatch(String),
    Protocol(String),
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

fn usage(message: impl Into<String>) -> Failure {
    Failure::Usage(message.into())
}

fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Invocation, Failure> {
    let command_arg = args.next().ok_or_else(|| usage("missing command"))?;
    let command = Command::parse(&command_arg).ok_or_else(|| {
        usage(format!(
            "unknown command '{}'",
            command_arg.to_string_lossy()
        ))
    })?;
    let mode =
        match (args.next(), args.next()) {
            (Some(flag), Some(value)) if flag == "--mode" => value
                .to_str()
                .and_then(OpenMode::parse)
                .ok_or_else(|| usage(format!("invalid mode '{}'", value.to_string_lossy())))?,
            _ => return Err(usage("expected --mode <default|multiprocess>")),
        };
    let busy_timeout = match (args.next(), args.next()) {
        (Some(flag), Some(value)) if flag == "--busy-timeout-ms" => value
            .to_str()
            .and_then(|text| text.parse::<u64>().ok())
            .map(Duration::from_millis)
            .ok_or_else(|| {
                usage(format!(
                    "invalid busy timeout '{}'",
                    value.to_string_lossy()
                ))
            })?,
        _ => return Err(usage("expected --busy-timeout-ms <MS>")),
    };
    let database = args.next().ok_or_else(|| usage("missing database path"))?;
    if let Some(extra) = args.next() {
        return Err(usage(format!(
            "unexpected argument '{}'",
            extra.to_string_lossy()
        )));
    }
    Ok(Invocation {
        command,
        config: ProbeConfig { mode, busy_timeout },
        database: PathBuf::from(database),
    })
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

fn elapsed_ms(started: Instant) -> String {
    format!("{:.1}", started.elapsed().as_secs_f64() * 1000.0)
}

fn to_hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text == "-" {
        return Some(Vec::new());
    }
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect()
}

async fn run(invocation: Invocation) -> Result<(), Failure> {
    let Invocation {
        command,
        config,
        database: path,
    } = invocation;
    report(&format!("config {config}"))?;
    if let Command::InspectExisting = command {
        return inspect_existing(&path, config).await;
    }
    let database = open(&path, config.mode).await?;
    let mut connection = connect(&database, config.busy_timeout)?;
    match command {
        Command::RoundtripWrite => {
            ensure_initialized(&connection).await?;
            let cases = binary_cases();
            for (key, bytes) in &cases {
                put_content(&connection, key, bytes).await?;
            }
            report(&format!("wrote {} cases", cases.len()))
        }
        Command::RoundtripVerify => roundtrip_verify(&connection).await,
        Command::TransactionCommit | Command::TransactionRollback => {
            ensure_initialized(&connection).await?;
            let transaction = begin_immediate(&mut connection).await?;
            put_interrupt_pair(&transaction).await?;
            if let Command::TransactionCommit = command {
                commit(transaction).await?;
            } else {
                rollback(transaction).await?;
            }
            report_visibility(interrupt_visibility(&connection).await?)
        }
        Command::Inspect => report_visibility(interrupt_visibility(&connection).await?),
        Command::Settings => {
            let settings = durability_settings(&connection).await?;
            report(&format!(
                "journal_mode={} synchronous={}",
                settings.journal_mode, settings.synchronous
            ))
        }
        Command::Initialize => initialize(&connection).await,
        Command::Session => session(&connection).await,
        Command::InspectExisting => Ok(()),
    }
}

async fn roundtrip_verify(connection: &Connection) -> Result<(), Failure> {
    let cases = binary_cases();
    for (key, expected) in &cases {
        let Some(stored) = read_content(connection, key).await? else {
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

async fn initialize(connection: &Connection) -> Result<(), Failure> {
    let precheck = readiness(connection).await?;
    report(&format!("precheck {precheck}"))?;
    let started = Instant::now();
    let outcome = match precheck {
        Readiness::Current => probe::InitOutcome::AlreadyCurrent,
        Readiness::Uninitialized => initialize_under_lock(connection).await?,
        Readiness::Version(version) => {
            return Err(ProbeError::UnsupportedSchemaVersion(version).into());
        }
    };
    report(&format!(
        "ok initialize outcome={outcome} elapsed_ms={}",
        elapsed_ms(started)
    ))
}

async fn inspect_existing(path: &std::path::Path, config: ProbeConfig) -> Result<(), Failure> {
    match open_existing(path, config).await? {
        ExistingOpen::Missing => report("missing"),
        ExistingOpen::SchemaNotCurrent(readiness) => {
            report(&format!("schema-not-current readiness={readiness}"))
        }
        ExistingOpen::Open {
            database: _database,
            connection,
        } => {
            let cookie = schema_cookie(&connection).await?;
            let visibility = interrupt_visibility(&connection).await?;
            report(&format!(
                "current schema_cookie={cookie} content={} head={}",
                presence(visibility.content),
                presence(visibility.head)
            ))
        }
    }
}

fn error_line(step: &str, error: &ProbeError, started: Instant, connection: &Connection) -> String {
    let kind = error
        .driver_kind()
        .map_or_else(|| "Probe".to_owned(), |kind| format!("{kind:?}"));
    let autocommit = connection
        .is_autocommit()
        .map_or_else(|_| "unknown".to_owned(), |value| value.to_string());
    format!(
        "err {step} {kind} elapsed_ms={} autocommit={autocommit} {error}",
        elapsed_ms(started)
    )
}

async fn write_pair(
    connection: &Connection,
    key: &str,
    head: &str,
    bytes: &[u8],
    started: Instant,
) -> String {
    let transaction = match begin_immediate_unchecked(connection).await {
        Ok(transaction) => transaction,
        Err(error) => return error_line("begin", &error, started, connection),
    };
    let written = match put_content(&transaction, key, bytes).await {
        Ok(()) => put_head(&transaction, head, key)
            .await
            .map_err(|error| ("head", error)),
        Err(error) => Err(("put", error)),
    };
    if let Err((step, error)) = written {
        let _ = rollback(transaction).await;
        return error_line(step, &error, started, connection);
    }
    match commit(transaction).await {
        Ok(()) => format!("ok write-pair elapsed_ms={}", elapsed_ms(started)),
        Err(error) => error_line("commit", &error, started, connection),
    }
}

async fn describe(connection: &Connection, key: &str, head: &str) -> Result<String, ProbeError> {
    let content = read_content(connection, key)
        .await?
        .map_or_else(|| "absent".to_owned(), |stored| hex_or_dash(&stored.bytes));
    let head = read_head(connection, head)
        .await?
        .unwrap_or_else(|| "absent".to_owned());
    Ok(format!("ok content={content} head={head}"))
}

fn hex_or_dash(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        "-".to_owned()
    } else {
        to_hex(bytes)
    }
}

fn respond(
    step: &str,
    result: Result<String, ProbeError>,
    started: Instant,
    connection: &Connection,
) -> Result<(), Failure> {
    match result {
        Ok(line) => report(&line),
        Err(error) => report(&error_line(step, &error, started, connection)),
    }
}

async fn session(connection: &Connection) -> Result<(), Failure> {
    let mut transaction: Option<Transaction<'_>> = None;
    report("ready")?;
    let mut line = String::new();
    loop {
        line.clear();
        if io::stdin().lock().read_line(&mut line)? == 0 {
            return Err(Failure::Protocol("stdin closed without quit".to_owned()));
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let started = Instant::now();
        match (words.as_slice(), transaction.is_some()) {
            (["quit"], _) => {
                if let Some(open) = transaction.take() {
                    rollback(open).await?;
                }
                return report("bye");
            }
            (["begin"], false) => match begin_immediate_unchecked(connection).await {
                Ok(open) => {
                    transaction = Some(open);
                    report(&format!("ok begin elapsed_ms={}", elapsed_ms(started)))?;
                }
                Err(error) => report(&error_line("begin", &error, started, connection))?,
            },
            (["commit"], true) => {
                let result = match transaction.take() {
                    Some(open) => commit(open).await,
                    None => Ok(()),
                };
                let result =
                    result.map(|()| format!("ok commit elapsed_ms={}", elapsed_ms(started)));
                respond("commit", result, started, connection)?;
            }
            (["rollback"], true) => {
                let result = match transaction.take() {
                    Some(open) => rollback(open).await,
                    None => Ok(()),
                };
                let result =
                    result.map(|()| format!("ok rollback elapsed_ms={}", elapsed_ms(started)));
                respond("rollback", result, started, connection)?;
            }
            (["put", key, hex], _) => {
                let bytes = from_hex(hex)
                    .ok_or_else(|| Failure::Protocol(format!("invalid hex '{hex}'")))?;
                let result = put_content(connection, key, &bytes)
                    .await
                    .map(|()| format!("ok put elapsed_ms={}", elapsed_ms(started)));
                respond("put", result, started, connection)?;
            }
            (["head", name, key], _) => {
                let result = put_head(connection, name, key)
                    .await
                    .map(|()| format!("ok head elapsed_ms={}", elapsed_ms(started)));
                respond("head", result, started, connection)?;
            }
            (["get", key, name], _) => {
                let result = describe(connection, key, name).await;
                respond("get", result, started, connection)?;
            }
            (["write-pair", key, name, hex], false) => {
                let bytes = from_hex(hex)
                    .ok_or_else(|| Failure::Protocol(format!("invalid hex '{hex}'")))?;
                report("started")?;
                let started = Instant::now();
                report(&write_pair(connection, key, name, &bytes, started).await)?;
            }
            (["init-hold"], false) => {
                let open = match begin_immediate_unchecked(connection).await {
                    Ok(open) => open,
                    Err(error) => {
                        report(&error_line("begin", &error, started, connection))?;
                        continue;
                    }
                };
                let recheck = readiness(&open).await?;
                if recheck == Readiness::Uninitialized {
                    probe::apply_schema(&open).await?;
                    transaction = Some(open);
                } else {
                    rollback(open).await?;
                }
                report(&format!("ok init-hold recheck={recheck}"))?;
            }
            _ => {
                return Err(Failure::Protocol(format!(
                    "unexpected session command '{}' (transaction open: {})",
                    line.trim_end(),
                    transaction.is_some()
                )));
            }
        }
    }
}

fn main() -> ExitCode {
    let outcome = parse(env::args_os().skip(1)).and_then(|invocation| block_on(run(invocation)));
    let mut stderr = io::stderr().lock();
    let written = match outcome {
        Ok(()) => return ExitCode::SUCCESS,
        Err(Failure::Usage(message)) => {
            let _ = write!(stderr, "error: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
        Err(Failure::Probe(error)) => writeln!(stderr, "error: {error}\ndetail: {error:?}"),
        Err(Failure::Mismatch(message)) => writeln!(stderr, "error: {message}"),
        Err(Failure::Protocol(message)) => writeln!(stderr, "error: session protocol: {message}"),
        Err(Failure::Io(error)) => writeln!(stderr, "error: probe I/O: {error}"),
    };
    drop(written);
    ExitCode::FAILURE
}
