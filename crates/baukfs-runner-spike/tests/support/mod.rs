use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

pub const REPLY_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(2);
const LONGEST_SCRATCH_PREFIX: usize = 60;

/// Per-test scratch tree: a workspace state directory kept outside the
/// agent-writable working directory, plus the fixture control socket.
pub struct Scratch {
    root: PathBuf,
}

impl Scratch {
    pub fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let mut base = env::temp_dir();
        // Unix socket paths are limited to about 104 bytes on macOS.
        if base.as_os_str().len() > LONGEST_SCRATCH_PREFIX {
            base = PathBuf::from("/tmp");
        }
        let root = base.join(format!(
            "brs-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("work")).expect("create scratch workdir");
        Self { root }
    }

    pub fn workdir(&self) -> PathBuf {
        self.root.join("work")
    }

    pub fn state(&self, workspace: &str) -> PathBuf {
        self.root.join("state").join(workspace)
    }

    pub fn socket(&self) -> PathBuf {
        self.root.join("ctl.sock")
    }

    pub fn read_work_file(&self, name: &str) -> String {
        fs::read_to_string(self.workdir().join(name)).unwrap_or_default()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_ascii_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

/// A fixture process connected over the control socket.
pub struct Fixture {
    pub role: String,
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub sid: i32,
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Fixture {
    pub fn send(&mut self, command: &str) -> String {
        writeln!(self.stream, "{command}").expect("send fixture command");
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => panic!("fixture {} closed before replying to {command}", self.role),
            Ok(_) => line.trim_end().to_owned(),
            Err(error) => panic!("fixture {} reply to {command}: {error}", self.role),
        }
    }

    /// Tells the fixture to exit and waits until its socket closes, which the
    /// kernel does while tearing the process down.
    pub fn exit(&mut self, code: u8) {
        writeln!(self.stream, "exit {code}").expect("send exit");
        self.wait_closed();
    }

    /// Waits for end of file on the control socket: the fixture has exited or
    /// been killed (socket descriptors are close-on-exec, so only this
    /// process holds it).
    pub fn wait_closed(&mut self) {
        let mut rest = String::new();
        match self.reader.read_line(&mut rest) {
            Ok(0) => {}
            Ok(_) => panic!("fixture {} sent unexpected {rest:?}", self.role),
            Err(error) => panic!("fixture {} did not close: {error}", self.role),
        }
    }
}

/// Accepts fixture connections and cleans up every fixture on drop.
pub struct Fixtures {
    listener: UnixListener,
    pending: HashMap<String, Fixture>,
    active: Vec<i32>,
    streams: Vec<UnixStream>,
}

impl Fixtures {
    pub fn listen(scratch: &Scratch) -> Self {
        let listener = UnixListener::bind(scratch.socket()).expect("bind control socket");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        Self {
            listener,
            pending: HashMap::new(),
            active: Vec::new(),
            streams: Vec::new(),
        }
    }

    fn accept_one(&mut self, deadline: Instant) -> Fixture {
        let stream = loop {
            match self.listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "no fixture connected in time");
                    thread::sleep(POLL);
                }
                Err(error) => panic!("accept fixture: {error}"),
            }
        };
        stream.set_nonblocking(false).expect("blocking stream");
        stream
            .set_read_timeout(Some(REPLY_TIMEOUT))
            .expect("read timeout");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut hello = String::new();
        reader.read_line(&mut hello).expect("read hello");
        let number = |key| {
            field(&hello, key)
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| panic!("hello lacks {key}: {hello:?}"))
        };
        let fixture = Fixture {
            role: field(&hello, "role").expect("hello role").to_owned(),
            pid: number("pid"),
            ppid: number("ppid"),
            pgid: number("pgid"),
            sid: number("sid"),
            stream: stream.try_clone().expect("clone stream"),
            reader,
        };
        self.active.push(fixture.pid);
        self.streams.push(stream);
        fixture
    }

    /// Returns the next fixture that announces `role`.
    pub fn expect(&mut self, role: &str) -> Fixture {
        if let Some(fixture) = self.pending.remove(role) {
            return fixture;
        }
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            let fixture = self.accept_one(deadline);
            if fixture.role == role {
                return fixture;
            }
            self.pending.insert(fixture.role.clone(), fixture);
        }
    }
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        for (pid, stream) in self.active.iter().zip(&mut self.streams) {
            let _ = writeln!(stream, "exit 0");
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut reader = BufReader::new(&*stream);
            let mut line = String::new();
            let closed = loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break true,
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break true,
                    Err(error) if error.raw_os_error() == Some(libc::ECONNRESET) => break true,
                    Err(_) => break false,
                }
            };
            if !closed {
                // The socket is still open, so this pid still names the fixture.
                // SAFETY: kill has no memory-safety preconditions.
                unsafe {
                    libc::kill(*pid, libc::SIGKILL);
                }
            }
        }
    }
}

/// A running `spike-controller` process driven over stdin/stdout.
pub struct Controller {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    pub ready: String,
}

pub struct ControllerArgs<'a> {
    pub scratch: &'a Scratch,
    pub workspace: &'a str,
    pub cgroup: bool,
    /// Runs the controller inside a new delegated systemd user scope.
    pub systemd_scope: bool,
}

impl Controller {
    pub fn start(args: &ControllerArgs<'_>) -> Self {
        let controller = env!("CARGO_BIN_EXE_spike-controller");
        let mut options: Vec<OsString> = Vec::new();
        options.extend(["--state".into(), args.scratch.state(args.workspace).into()]);
        options.extend(["--workdir".into(), args.scratch.workdir().into()]);
        options.extend([
            "--fixture".into(),
            env!("CARGO_BIN_EXE_spike-fixture").into(),
        ]);
        if args.cgroup {
            options.push("--cgroup".into());
        }
        let mut command = if args.systemd_scope {
            let mut command = Command::new("systemd-run");
            command
                .args(["--user", "--scope", "--quiet", "-p", "Delegate=yes", "--"])
                .arg(controller);
            command
        } else {
            Command::new(controller)
        };
        let mut child = command
            .args(options)
            .env("BAUKFS_SPIKE_CTL", args.scratch.socket())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn controller");
        let stdout = child.stdout.take().expect("controller stdout");
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        let mut controller = Self {
            child,
            stdin,
            lines,
            ready: String::new(),
        };
        controller.ready = controller.next_line("startup");
        controller
    }

    fn next_line(&mut self, context: &str) -> String {
        match self.lines.recv_timeout(REPLY_TIMEOUT) {
            Ok(line) => line,
            Err(error) => panic!("controller gave no reply to {context}: {error}"),
        }
    }

    pub fn send(&mut self, command: &str) -> String {
        let stdin = self.stdin.as_mut().expect("controller stdin");
        writeln!(stdin, "{command}").expect("send controller command");
        self.next_line(command)
    }

    pub fn pid(&self) -> i32 {
        i32::try_from(self.child.id()).expect("pid fits i32")
    }

    /// Sends `SIGKILL` to the controller and reaps it.
    pub fn kill(&mut self) -> ExitStatus {
        self.child.kill().expect("kill controller");
        self.child.wait().expect("reap controller")
    }

    pub fn wait_exit(&mut self) -> ExitStatus {
        self.stdin = None;
        self.child.wait().expect("reap controller")
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn controller(scratch: &Scratch, workspace: &str) -> Controller {
    Controller::start(&ControllerArgs {
        scratch,
        workspace,
        cgroup: false,
        systemd_scope: false,
    })
}

pub fn assert_starts_with(line: &str, prefix: &str) {
    assert!(
        line.starts_with(prefix),
        "expected {prefix:?}, got {line:?}"
    );
}
