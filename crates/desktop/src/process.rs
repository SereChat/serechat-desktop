//! Commands the agent starts and checks on later: dev servers, watchers,
//! slow builds. Their output is collected in the background; the agent reads
//! what is new and stops them when done. Each belongs to the conversation
//! that started it, and every one is stopped when the app exits.
//!
//! Stopping ends the whole process tree (a shell's children included) with
//! the tools the platform ships with: `taskkill` on Windows, `kill` on a
//! process group elsewhere.

use std::fmt::Write as _;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::platform::no_window;

/// Most processes running at once, across all conversations.
const MAX_RUNNING: usize = 8;
/// Output kept per process; older output is dropped.
const KEEP: usize = 1 << 20;
/// How long a new process may print before `start` reports back.
const START_WAIT: Duration = Duration::from_secs(3);
/// Longest `read` may wait for new output.
pub const MAX_WAIT_SECS: u64 = 30;

/// A process's output, trimmed to its last [`KEEP`] bytes.
#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    /// Bytes ever written, including those dropped.
    total: usize,
}

struct Process {
    id: u32,
    /// Conversation that started it.
    owner: u64,
    child: Child,
    output: Arc<Mutex<Output>>,
    /// Threads copying its stdout and stderr into `output`.
    readers: Vec<JoinHandle<()>>,
    /// Output bytes (of `total`) the agent has read.
    read: usize,
}

static PROCESSES: Mutex<Vec<Process>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

fn processes() -> MutexGuard<'static, Vec<Process>> {
    PROCESSES.lock().unwrap_or_else(PoisonError::into_inner)
}

fn lock(output: &Mutex<Output>) -> MutexGuard<'_, Output> {
    output.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A shell running `command` in `root`, in a process group of its own on
/// Unix so [`kill_tree`] reaches everything it starts.
pub fn shell(root: &Path, command: &str) -> Command {
    let mut process = if cfg!(target_os = "windows") {
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-Command", command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    };
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut process, 0);
    no_window(&mut process).current_dir(root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    process
}

/// Ends `child` and everything it started, then reaps it.
pub fn kill_tree(child: &mut Child) {
    let pid = child.id().to_string();
    if cfg!(target_os = "windows") {
        let _ = no_window(Command::new("taskkill").args(["/PID", &pid, "/T", "/F"])).stdout(Stdio::null()).stderr(Stdio::null()).status();
    } else {
        // Ask politely, then insist.
        let group = format!("-{pid}");
        let signal = |name: &str| Command::new("kill").args(["-s", name, "--", &group]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        let _ = signal("TERM");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = signal("KILL");
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Copies `pipe` into `output` until it closes.
fn collect(pipe: Option<impl Read + Send + 'static>, output: Arc<Mutex<Output>>) -> Option<JoinHandle<()>> {
    let mut pipe = pipe?;
    std::thread::Builder::new()
        .name("serechat-process-output".into())
        .spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n @ 1..) = pipe.read(&mut buf) {
                let mut output = lock(&output);
                output.bytes.extend_from_slice(&buf[..n]);
                output.total += n;
                let excess = output.bytes.len().saturating_sub(KEEP);
                output.bytes.drain(..excess);
            }
        })
        .ok()
}

/// Starts `command` in `root` for conversation `owner` and reports its first
/// seconds of output.
///
/// # Errors
/// Too many processes are running, or the command could not start.
pub fn start(root: &Path, owner: u64, command: &str, cancel: &AtomicBool) -> Result<String, String> {
    let running = processes().iter_mut().map(|p| matches!(p.child.try_wait(), Ok(None))).filter(|running| *running).count();
    if running >= MAX_RUNNING {
        return Err(format!("{MAX_RUNNING} processes are already running; stop one with stop_process first."));
    }
    let mut child = shell(root, command).spawn().map_err(|e| format!("The command could not start: {e}"))?;
    let output = Arc::new(Mutex::new(Output::default()));
    // The threads end on their own when the process closes its pipes.
    let readers = [collect(child.stdout.take(), Arc::clone(&output)), collect(child.stderr.take(), Arc::clone(&output))];
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    processes().push(Process { id, owner, child, output, readers: readers.into_iter().flatten().collect(), read: 0 });
    let report = read(owner, id, START_WAIT, cancel)?;
    Ok(format!("Started process {id}. Check on it with read_process and end it with stop_process.\n{report}"))
}

/// New output of process `id` and whether it still runs. Waits up to `wait`
/// for something to happen (output or an exit), then a moment more so a
/// burst of output arrives whole. An exited process is forgotten once all
/// its output has been read.
///
/// # Errors
/// There is no such process in this conversation.
pub fn read(owner: u64, id: u32, wait: Duration, cancel: &AtomicBool) -> Result<String, String> {
    let deadline = Instant::now() + wait;
    let mut report_at: Option<Instant> = None;
    loop {
        {
            let mut list = processes();
            let index = list.iter().position(|p| p.id == id && p.owner == owner).ok_or_else(|| format!("There is no process {id}."))?;
            let process = &mut list[index];
            let exited = matches!(process.child.try_wait(), Ok(Some(_)));
            let now = Instant::now();
            if report_at.is_none() && (exited || lock(&process.output).total > process.read) {
                report_at = Some(now + Duration::from_millis(200));
            }
            if report_at.is_some_and(|at| now >= at) || now >= deadline || cancel.load(Ordering::Relaxed) {
                let text = report(process);
                // Once its pipes have closed, nothing more can arrive.
                if exited && process.readers.iter().all(JoinHandle::is_finished) && lock(&process.output).total == process.read {
                    list.remove(index);
                }
                return Ok(text);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The status line and the output since the last read.
fn report(process: &mut Process) -> String {
    let status = process.child.try_wait().ok().flatten();
    let output = lock(&process.output);
    let kept_from = output.total - output.bytes.len();
    let start = process.read.max(kept_from) - kept_from;
    let mut text = match status {
        None => "running".to_owned(),
        Some(s) => format!("exited with code {}", s.code().map_or_else(|| "none (killed by a signal)".into(), |c| c.to_string())),
    };
    if process.read < kept_from {
        let _ = write!(text, "\n({} earlier bytes were dropped)", kept_from - process.read);
    }
    let new = String::from_utf8_lossy(&output.bytes[start..]);
    text.push('\n');
    text.push_str(if new.trim().is_empty() { "(no new output)" } else { new.trim_end() });
    process.read = output.total;
    text
}

/// Stops process `id` and returns its last output.
///
/// # Errors
/// There is no such process in this conversation.
pub fn stop(owner: u64, id: u32) -> Result<String, String> {
    let mut process = {
        let mut list = processes();
        let index = list.iter().position(|p| p.id == id && p.owner == owner).ok_or_else(|| format!("There is no process {id}."))?;
        list.remove(index)
    };
    kill_tree(&mut process.child);
    Ok(format!("Stopped process {id}.\n{}", report(&mut process)))
}

/// Stops every process of conversation `owner` (when it is deleted).
pub fn stop_owner(conversation: u64) {
    let theirs: Vec<Process> = {
        let mut list = processes();
        let (theirs, others) = std::mem::take(&mut *list).into_iter().partition(|p| p.owner == conversation);
        *list = others;
        theirs
    };
    for mut process in theirs {
        kill_tree(&mut process.child);
    }
}

/// Stops every process; called when the app exits.
pub fn stop_all() {
    let all = std::mem::take(&mut *processes());
    for mut process in all {
        kill_tree(&mut process.child);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processes_run_report_and_stop() {
        let root = std::env::temp_dir();
        let cancel = AtomicBool::new(false);
        let (owner, other) = (u64::MAX - 1, u64::MAX - 2);
        let quick = if cfg!(target_os = "windows") { "Write-Output ready" } else { "echo ready" };
        let started = start(&root, owner, quick, &cancel).unwrap();
        assert!(started.contains("ready"), "{started}");
        let id: u32 = started.trim_start_matches("Started process ").split('.').next().unwrap().parse().unwrap();
        // Reading until it has exited and said everything forgets it.
        let mut reads = 0;
        while read(owner, id, Duration::from_secs(1), &cancel).is_ok() {
            reads += 1;
            assert!(reads < 20, "an exited process is forgotten once read");
        }

        let slow = if cfg!(target_os = "windows") { "Write-Output up; Start-Sleep 60" } else { "echo up; sleep 60" };
        let started = start(&root, owner, slow, &cancel).unwrap();
        assert!(started.contains("running"), "{started}");
        let id: u32 = started.trim_start_matches("Started process ").split('.').next().unwrap().parse().unwrap();
        assert!(read(other, id, Duration::ZERO, &cancel).is_err(), "other conversations can't see it");
        assert!(read(owner, id, Duration::ZERO, &cancel).unwrap().contains("(no new output)"));
        assert!(stop(owner, id).unwrap().starts_with(&format!("Stopped process {id}.")));
        assert!(stop(owner, id).is_err());
    }
}
