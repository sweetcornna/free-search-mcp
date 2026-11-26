//! Supervised child processes: cleared environment, piped stdio, RSS
//! watchdog, wall-clock timeout, output cap, cancellation (`SPEC.md` §7.3 S3/S11).

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// Environment variables a supervised child may inherit (S3).
pub const KEPT_ENV: [&str; 3] = ["PATH", "TMPDIR", "LANG"];

/// Why the supervisor killed a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillReason {
    /// Resident memory passed the cap.
    Memory { rss: u64, limit: u64 },
    /// Resident memory grew too fast.
    Growth { delta: u64, window: Duration },
    /// Wall clock ran out.
    Timeout,
    /// The caller's cancellation token fired.
    Cancelled,
    /// stdout passed its cap.
    OutputTooLarge,
    /// The child had reported completion but did not exit within the grace period.
    Grace,
}

/// How to run and police one child.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub stdin: Arc<[u8]>,
    pub timeout: Duration,
    pub rss_limit: u64,
    pub growth_limit: u64,
    pub growth_window: Duration,
    pub poll: Duration,
    pub max_stdout: usize,
    /// A stdout line starting with this means "finished": the child then
    /// gets `grace` to exit before it is killed (reason [`KillReason::Grace`]).
    pub done_marker: Option<&'static [u8]>,
    pub grace: Duration,
}

/// What a supervised run produced.
#[derive(Debug, Clone)]
pub struct RawRun {
    pub stdout: Vec<u8>,
    /// Last 64 KiB of stderr.
    pub stderr_tail: Vec<u8>,
    pub status: Option<ExitStatus>,
    pub killed: Option<KillReason>,
    pub elapsed: Duration,
    pub peak_rss: u64,
    pub pid: Option<u32>,
}

impl RawRun {
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr_tail).trim().to_owned()
    }

    /// `exit 3` / `signal 9` / `killed (..)`.
    pub fn describe_exit(&self) -> String {
        if let Some(k) = self.killed {
            return format!("killed ({k:?})");
        }
        match self.status {
            Some(s) => match s.code() {
                Some(c) => format!("exit {c}"),
                None => signal_of(&s)
                    .map_or_else(|| "abnormal exit".to_owned(), |sig| format!("signal {sig}")),
            },
            None => "no exit status".to_owned(),
        }
    }
}

#[cfg(unix)]
fn signal_of(s: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    s.signal()
}

#[cfg(not(unix))]
fn signal_of(_s: &ExitStatus) -> Option<i32> {
    None
}

/// Resident size of `pid` in bytes, if this platform can tell.
/// macOS: `max(ri_resident_size, ri_phys_footprint)` (the footprint also
/// counts compressed pages, which resident size hides under memory pressure).
#[cfg(target_os = "macos")]
pub fn rss_bytes(pid: u32) -> Option<u64> {
    use libproc::pid_rusage::{RUsageInfoV2, pidrusage};
    let r: RUsageInfoV2 = pidrusage(i32::try_from(pid).ok()?).ok()?;
    Some(r.ri_resident_size.max(r.ri_phys_footprint))
}

/// Linux: `/proc/<pid>/statm` resident pages × page size.
#[cfg(target_os = "linux")]
pub fn rss_bytes(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let pages: u64 = s.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    Some(pages.saturating_mul(u64::try_from(page).ok().filter(|p| *p > 0).unwrap_or(4096)))
}

/// Other platforms (Windows is P1): no RSS watch.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn rss_bytes(_pid: u32) -> Option<u64> {
    None
}

/// Does a line of `buf` starting at or after byte `from` begin with `marker`?
fn line_marker_from(buf: &[u8], from: usize, marker: &[u8]) -> bool {
    (from..buf.len()).any(|i| (i == 0 || buf[i - 1] == b'\n') && buf[i..].starts_with(marker))
}

/// Spawn `spec.program` with a cleared environment (only [`KEPT_ENV`] and
/// `extra_env` pass), piped stdin/stdout/stderr, and police it until it
/// exits or is killed. The child is always reaped before this returns.
pub async fn run_child(
    spec: &ChildSpec,
    extra_env: &[(&str, String)],
    cancel: &CancellationToken,
) -> std::io::Result<RawRun> {
    let started = Instant::now();
    let mut cmd = tokio::process::Command::new(&spec.program);
    cmd.args(&spec.args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in KEPT_ENV {
        if let Some(v) = std::env::var_os(key) {
            cmd.env(key, v);
        }
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;
    let pid = child.id();

    let stdin = child.stdin.take();
    let input = Arc::clone(&spec.stdin);
    let writer = tokio::spawn(async move {
        if let Some(mut stdin) = stdin {
            // BrokenPipe just means the child stopped reading; its exit
            // status, not this write, decides the outcome.
            let _ = stdin.write_all(&input).await;
            let _ = stdin.shutdown().await;
        }
    });

    let done_seen = Arc::new(AtomicBool::new(false));
    let overflow = Arc::new(AtomicBool::new(false));
    let stdout_len = Arc::new(AtomicU64::new(0));
    let stdout = child.stdout.take();
    let (max_out, marker) = (spec.max_stdout, spec.done_marker);
    let (done_w, overflow_w, len_w) = (
        Arc::clone(&done_seen),
        Arc::clone(&overflow),
        Arc::clone(&stdout_len),
    );
    let reader = tokio::spawn(async move {
        let mut buf = Vec::new();
        let Some(mut out) = stdout else { return buf };
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match out.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if buf.len() + n > max_out {
                        overflow_w.store(true, Ordering::SeqCst);
                        break;
                    }
                    let scan_from = buf.len().saturating_sub(marker.map_or(0, <[u8]>::len));
                    buf.extend_from_slice(&chunk[..n]);
                    len_w.store(buf.len() as u64, Ordering::SeqCst);
                    if let Some(m) = marker
                        && !done_w.load(Ordering::SeqCst)
                        && line_marker_from(&buf, scan_from, m)
                    {
                        done_w.store(true, Ordering::SeqCst);
                    }
                }
            }
        }
        buf
    });
    let stderr = child.stderr.take();
    let err_reader = tokio::spawn(async move {
        let mut tail: Vec<u8> = Vec::new();
        let Some(mut err) = stderr else { return tail };
        let mut chunk = vec![0u8; 16 * 1024];
        while let Ok(n) = err.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&chunk[..n]);
            if tail.len() > 64 * 1024 {
                let cut = tail.len() - 64 * 1024;
                tail.drain(..cut);
            }
        }
        tail
    });

    let mut killed: Option<KillReason> = None;
    let mut peak_rss = 0u64;
    let mut samples: std::collections::VecDeque<(Instant, u64)> = std::collections::VecDeque::new();
    let mut done_at: Option<Instant> = None;
    let deadline = started + spec.timeout;
    let mut tick = tokio::time::interval(spec.poll.max(Duration::from_millis(10)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let status = loop {
        tokio::select! {
            st = child.wait() => break st.ok(),
            () = cancel.cancelled() => { killed = Some(KillReason::Cancelled); }
            _ = tick.tick() => {
                let now = Instant::now();
                if overflow.load(Ordering::SeqCst) {
                    killed = Some(KillReason::OutputTooLarge);
                } else if now >= deadline {
                    killed = Some(KillReason::Timeout);
                } else if let Some(rss) = pid.and_then(rss_bytes) {
                    peak_rss = peak_rss.max(rss);
                    while samples.front().is_some_and(|(t, _)| now.duration_since(*t) > spec.growth_window) {
                        samples.pop_front();
                    }
                    let base = samples.iter().map(|(_, r)| *r).min().unwrap_or(rss);
                    samples.push_back((now, rss));
                    if rss > spec.rss_limit {
                        killed = Some(KillReason::Memory { rss, limit: spec.rss_limit });
                    } else if rss.saturating_sub(base) > spec.growth_limit {
                        killed = Some(KillReason::Growth { delta: rss - base, window: spec.growth_window });
                    }
                }
                if killed.is_none() && done_seen.load(Ordering::SeqCst) {
                    let at = *done_at.get_or_insert(now);
                    if now.duration_since(at) >= spec.grace {
                        killed = Some(KillReason::Grace);
                    }
                }
            }
        }
        if killed.is_some() {
            let _ = child.start_kill();
            break child.wait().await.ok();
        }
    };
    writer.abort();
    let join = |h: tokio::task::JoinHandle<Vec<u8>>| async move {
        match tokio::time::timeout(Duration::from_secs(2), h).await {
            Ok(Ok(v)) => v,
            _ => Vec::new(),
        }
    };
    let stdout = join(reader).await;
    let stderr_tail = join(err_reader).await;
    if killed.is_none() && overflow.load(Ordering::SeqCst) {
        killed = Some(KillReason::OutputTooLarge);
    }
    Ok(RawRun {
        stdout,
        stderr_tail,
        status,
        killed,
        elapsed: started.elapsed(),
        peak_rss,
        pid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_must_start_a_line() {
        assert!(line_marker_from(b"FSM done", 0, b"FSM"));
        assert!(line_marker_from(b"noise\nFSM done", 3, b"FSM"));
        assert!(!line_marker_from(b"noise FSM done", 0, b"FSM"));
        assert!(!line_marker_from(b"FSM\nnoise", 1, b"FSM"));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn own_rss_is_readable() {
        let rss = rss_bytes(std::process::id()).expect("rss of self");
        assert!(rss > 1 << 20, "{rss}");
    }
}
