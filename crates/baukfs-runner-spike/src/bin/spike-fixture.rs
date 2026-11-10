//! Controllable workload process for the spike's integration tests.
//!
//! Connects to the Unix socket named by `BAUKFS_SPIKE_CTL`, announces its
//! identity, then executes one command per line and acknowledges it. Exits
//! when the socket reaches end of file, so a dropped test harness never
//! leaves fixtures waiting forever.

use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{self, Command, ExitCode};

const HIGHEST_SCANNED_FD: RawFd = 1024;

fn identity() -> String {
    // SAFETY: these calls take no pointers and cannot fail for the calling process.
    let (process, parent, group, session) = unsafe {
        (
            libc::getpid(),
            libc::getppid(),
            libc::getpgid(0),
            libc::getsid(0),
        )
    };
    format!("pid={process} ppid={parent} pgid={group} sid={session}")
}

fn os_error() -> String {
    io::Error::last_os_error().to_string()
}

fn redirect_stdio_to_null() -> io::Result<()> {
    let null = File::open("/dev/null")?;
    for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: dup2 takes no pointers; both descriptors are valid.
        if unsafe { libc::dup2(null.as_raw_fd(), target) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn close_inherited_fds(keep: RawFd) {
    for fd in 3..=HIGHEST_SCANNED_FD {
        if fd != keep {
            // SAFETY: closing an unowned or invalid descriptor is harmless here;
            // this process holds no other std-owned descriptors at this point.
            unsafe {
                libc::close(fd);
            }
        }
    }
}

fn execute(role: &str, workdir: &Path, socket: RawFd, line: &str) -> Option<String> {
    let mut words = line.split_ascii_whitespace();
    let reply = match words.next()? {
        "write" => {
            let name = words.next()?;
            let result = OpenOptions::new()
                .create(true)
                .append(true)
                .open(workdir.join(name))
                .and_then(|mut file| {
                    writeln!(file, "{role} {}", process::id())?;
                    file.sync_all()
                });
            match result {
                Ok(()) => format!("wrote {name}"),
                Err(error) => format!("error write {error}"),
            }
        }
        "spawn" => {
            let child_role = words.next()?;
            let spawned =
                env::current_exe().and_then(|exe| Command::new(exe).arg(child_role).spawn());
            match spawned {
                Ok(child) => format!("spawned pid={}", child.id()),
                Err(error) => format!("error spawn {error}"),
            }
        }
        "setsid" => {
            // SAFETY: setsid takes no arguments.
            if unsafe { libc::setsid() } == -1 {
                format!("error setsid {}", os_error())
            } else {
                format!("detached {}", identity())
            }
        }
        "close-stdio" => match redirect_stdio_to_null() {
            Ok(()) => "closed-stdio".to_owned(),
            Err(error) => format!("error close-stdio {error}"),
        },
        "close-fds" => {
            close_inherited_fds(socket);
            "closed-fds".to_owned()
        }
        "ignore-term" => {
            // SAFETY: installing SIG_IGN has no handler code to make safe.
            if unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) } == libc::SIG_ERR {
                format!("error ignore-term {}", os_error())
            } else {
                "ignoring-term".to_owned()
            }
        }
        "exit" => {
            let code: u8 = words.next()?.parse().ok()?;
            process::exit(i32::from(code));
        }
        other => format!("error unknown-command {other}"),
    };
    Some(reply)
}

fn main() -> ExitCode {
    let Some(role) = env::args().nth(1) else {
        eprintln!("usage: spike-fixture <role>");
        return ExitCode::from(2);
    };
    let (Some(socket_path), Some(workdir)) = (
        env::var_os("BAUKFS_SPIKE_CTL"),
        env::var_os("BAUKFS_SPIKE_WORKDIR").map(PathBuf::from),
    ) else {
        eprintln!("spike-fixture: BAUKFS_SPIKE_CTL and BAUKFS_SPIKE_WORKDIR are required");
        return ExitCode::from(2);
    };
    let stream = match UnixStream::connect(&socket_path) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("spike-fixture {role}: cannot connect: {error}");
            return ExitCode::from(2);
        }
    };
    let _ = writeln!(io::stdout(), "fixture {role} started");
    let _ = writeln!(io::stderr(), "fixture {role} diagnostics");

    let socket = stream.as_raw_fd();
    let mut writer = &stream;
    if writeln!(writer, "hello role={role} {}", identity()).is_err() {
        return ExitCode::FAILURE;
    }
    for line in BufReader::new(&stream).lines() {
        let Ok(line) = line else {
            return ExitCode::FAILURE;
        };
        let reply = execute(&role, &workdir, socket, &line)
            .unwrap_or_else(|| format!("error malformed {line}"));
        if writeln!(writer, "{reply}").is_err() {
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
