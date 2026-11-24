//! Process handling for `media-control`: one-shot commands and the live
//! stream. Everything is plain `std::process`, so it compiles and is tested
//! on every platform; the backend itself is only wired on macOS.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const BINARY: &str = "media-control";
pub(crate) const GET_ARGS: &[&str] = &["get", "--micros", "--no-artwork"];
const STREAM_ARGS: &[&str] = &["stream", "--no-diff", "--no-artwork", "--micros"];
const POLL_SLICE: Duration = Duration::from_millis(25);

pub(crate) fn find_binary() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(BINARY))
        .find(|candidate| candidate.is_file())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NextLine {
    Line(String),
    Timeout,
    Closed,
}

pub(crate) trait StreamSource: Send {
    fn next_line(&mut self, wait: Duration) -> NextLine;
    fn shutdown(&mut self);
}

pub(crate) trait StreamFactory: Send + Sync {
    fn spawn(&self) -> Result<Box<dyn StreamSource>, String>;
}

pub(crate) struct ProcessFactory {
    pub binary: PathBuf,
}

impl StreamFactory for ProcessFactory {
    fn spawn(&self) -> Result<Box<dyn StreamSource>, String> {
        ProcessStream::spawn(&self.binary).map(|source| Box::new(source) as Box<dyn StreamSource>)
    }
}

struct ProcessStream {
    child: Child,
    lines: Receiver<String>,
    reader: Option<thread::JoinHandle<()>>,
    stderr: Option<thread::JoinHandle<()>>,
}

impl ProcessStream {
    fn spawn(binary: &Path) -> Result<Self, String> {
        Self::spawn_with(binary, STREAM_ARGS)
    }

    fn spawn_with(binary: &Path, args: &[&str]) -> Result<Self, String> {
        let mut child = Command::new(binary)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("spawn {}: {error}", binary.display()))?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        let (tx, lines) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr_thread = thread::spawn(move || {
            let mut pipe = stderr;
            let mut buffer = String::new();
            let _ = pipe.read_to_string(&mut buffer);
            let message = buffer.trim();
            if !message.is_empty() {
                tracing::debug!(stderr = %message, "mediaremote: adapter stderr");
            }
        });

        Ok(Self {
            child,
            lines,
            reader: Some(reader),
            stderr: Some(stderr_thread),
        })
    }
}

impl StreamSource for ProcessStream {
    fn next_line(&mut self, wait: Duration) -> NextLine {
        match self.lines.recv_timeout(wait) {
            Ok(line) => NextLine::Line(line),
            Err(RecvTimeoutError::Timeout) => NextLine::Timeout,
            Err(RecvTimeoutError::Disconnected) => NextLine::Closed,
        }
    }

    fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Reap only the direct child. A grandchild (the adapter runs perl)
        // can keep the pipes open, and joining the reader threads would then
        // block shutdown until it exits. Detached threads hold no locks and
        // end on EOF once every writer closes.
        self.reader = None;
        self.stderr = None;
    }
}

impl Drop for ProcessStream {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Run one bounded command and return its stdout. stderr is drained on a
/// separate thread so a chatty adapter cannot fill the pipe and deadlock.
pub(crate) fn run_once(binary: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("spawn {}: {error}", binary.display()))?;

    let mut stderr_pipe = child.stderr.take().expect("stderr is piped");
    let (stderr_tx, stderr_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buffer = String::new();
        let _ = stderr_pipe.read_to_string(&mut buffer);
        let _ = stderr_tx.send(buffer);
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{} timed out after {timeout:?}", args.join(" ")));
                }
                thread::sleep(POLL_SLICE);
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("wait failed: {error}"));
            }
        }
    };

    let _ = child.kill();
    let _ = child.wait();
    let stdout = child
        .stdout
        .take()
        .map(|mut pipe| {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            String::from_utf8_lossy(&buffer).into_owned()
        })
        .unwrap_or_default();
    // Bounded: a grandchild holding stderr must not stall the command.
    let stderr = stderr_rx
        .recv_timeout(Duration::from_millis(200))
        .unwrap_or_default();

    if status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "{} exited with {:?}: {}",
            args.join(" "),
            status.code(),
            stderr.trim()
        ))
    }
}

/// Collect a few raw stream frames for `lyrachi debug mediaremote`.
pub(crate) fn capture_frames(binary: &Path, max: usize, deadline: Duration) -> Vec<String> {
    let Ok(mut source) = ProcessStream::spawn(binary) else {
        return Vec::new();
    };
    let started = Instant::now();
    let mut frames = Vec::new();
    while frames.len() < max && started.elapsed() < deadline {
        match source.next_line(Duration::from_millis(200)) {
            NextLine::Line(line) => frames.push(line),
            NextLine::Timeout => {}
            NextLine::Closed => break,
        }
    }
    source.shutdown();
    frames
}

#[cfg(test)]
mod tests {
    // Every test in this module spawns a Unix shell, so on Windows nothing
    // from the parent module is referenced and the glob would be unused.
    #[cfg(unix)]
    use super::*;

    #[cfg(unix)]
    fn run_shell(script: &str, timeout: Duration) -> Result<String, String> {
        run_once(Path::new("/bin/sh"), &["-c", script], timeout)
    }

    #[cfg(unix)]
    fn shell_source(script: &str) -> ProcessStream {
        ProcessStream::spawn_with(Path::new("/bin/sh"), &["-c", script]).expect("shell source")
    }

    #[cfg(unix)]
    #[test]
    fn stream_source_yields_lines_then_reports_eof() {
        let mut source = shell_source("printf '%s\\n' 'first' 'second'");
        assert_eq!(
            source.next_line(Duration::from_secs(5)),
            NextLine::Line("first".to_string())
        );
        assert_eq!(
            source.next_line(Duration::from_secs(5)),
            NextLine::Line("second".to_string())
        );
        assert_eq!(source.next_line(Duration::from_secs(5)), NextLine::Closed);
        source.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn stream_shutdown_kills_a_long_running_child() {
        let mut source = shell_source("sleep 30");
        assert_eq!(
            source.next_line(Duration::from_millis(100)),
            NextLine::Timeout
        );
        let started = Instant::now();
        source.shutdown();
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn run_once_collects_stdout() {
        let output = run_shell("printf 'null'", Duration::from_secs(5)).expect("command");
        assert_eq!(output, "null");
    }

    #[cfg(unix)]
    #[test]
    fn run_once_reports_nonzero_exit_with_stderr() {
        let error = run_shell("echo boom >&2; exit 3", Duration::from_secs(5)).unwrap_err();
        assert!(error.contains("boom"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn run_once_times_out() {
        let error = run_shell("sleep 30", Duration::from_millis(100)).unwrap_err();
        assert!(error.contains("timed out"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn run_once_reports_spawn_failures() {
        let error = run_once(
            Path::new("/nonexistent/media-control"),
            &[],
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.contains("spawn"), "{error}");
    }
}
