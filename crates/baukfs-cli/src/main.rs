use std::env;
use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

const USAGE: &str = "\
Usage: baukfs-cli [COMMAND]

Commands:
  help    Print this help

Options:
  -h, --help    Print this help
";

enum Command {
    Help,
}

enum CliError {
    UnexpectedArgument(OsString),
}

fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Command, CliError> {
    let command = match args.next() {
        None => Command::Help,
        Some(arg) if arg == "help" || arg == "-h" || arg == "--help" => Command::Help,
        Some(arg) => return Err(CliError::UnexpectedArgument(arg)),
    };
    match args.next() {
        None => Ok(command),
        Some(arg) => Err(CliError::UnexpectedArgument(arg)),
    }
}

fn main() -> ExitCode {
    match parse(env::args_os().skip(1)) {
        Ok(Command::Help) => {
            let mut stdout = io::stdout().lock();
            match write!(stdout, "baukfs-cli {}\n\n{USAGE}", baukfs::VERSION) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::FAILURE,
            }
        }
        Err(CliError::UnexpectedArgument(arg)) => {
            let mut stderr = io::stderr().lock();
            let _ = write!(
                stderr,
                "error: unexpected argument '{}'\n\n{USAGE}",
                arg.to_string_lossy()
            );
            ExitCode::from(2)
        }
    }
}
