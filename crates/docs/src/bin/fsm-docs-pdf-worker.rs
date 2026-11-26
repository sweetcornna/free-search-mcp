//! Stand-alone PDF worker binary for this crate's integration tests (the
//! server binary runs the same entry point as its hidden `__pdf-worker`
//! subcommand). The `--selftest-*` hooks exist only here, never in
//! [`fsm_docs::DocService::run_pdf_worker_cli`]:
//!
//! - `--selftest-env`: report the environment variable names and whether
//!   stdin/stdout/stderr are pipes, as one protocol `error` line;
//! - `--selftest-alloc <MiB>`: allocate and touch incompressible memory in
//!   16 MiB steps (the RSS watchdog must kill it);
//! - `--selftest-sleep <ms>`: sleep (the wall clock must kill it);
//! - `--selftest-noise`: print non-protocol output and exit 0.

use std::io::Write;
use std::process::ExitCode;

fn report(kind: &str, payload: serde_json::Value) -> ExitCode {
    let msg = fsm_docs::testing::WorkerMsg::Error(fsm_docs::testing::ErrorMsg {
        kind: kind.to_owned(),
        error: payload.to_string(),
    });
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", msg.to_line());
    let _ = out.flush();
    ExitCode::from(fsm_docs::testing::exit::FAILED)
}

#[cfg(unix)]
fn is_pipe(fd: i32) -> bool {
    // SAFETY: fstat writes only into the zeroed struct we own.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(fd, &mut st) == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFIFO
    }
}

#[cfg(not(unix))]
fn is_pipe(_fd: i32) -> bool {
    false
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str);
    match arg(0) {
        Some("--selftest-env") => {
            let mut names: Vec<String> = std::env::vars_os()
                .map(|(k, _)| k.to_string_lossy().into_owned())
                .collect();
            names.sort();
            report(
                "selftest_env",
                serde_json::json!({
                    "env": names,
                    "stdinPipe": is_pipe(0),
                    "stdoutPipe": is_pipe(1),
                    "stderrPipe": is_pipe(2),
                }),
            )
        }
        Some("--selftest-alloc") => {
            let mib: usize = arg(1).and_then(|s| s.parse().ok()).unwrap_or(2048);
            let mut keep: Vec<Vec<u64>> = Vec::new();
            let mut x = 0x2545_f491_4f6c_dd1du64;
            for _ in 0..mib / 16 {
                let mut block = vec![0u64; (16 << 20) / 8];
                for w in &mut block {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    *w = x;
                }
                keep.push(block);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            std::thread::sleep(std::time::Duration::from_secs(30));
            ExitCode::from(u8::try_from(keep.len() % 2).unwrap_or(0))
        }
        Some("--selftest-sleep") => {
            let ms: u64 = arg(1).and_then(|s| s.parse().ok()).unwrap_or(60_000);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            ExitCode::SUCCESS
        }
        Some("--selftest-noise") => {
            let _ = writeln!(std::io::stdout(), "running 0 tests\n\ntest result: ok.");
            ExitCode::SUCCESS
        }
        _ => fsm_docs::DocService::run_pdf_worker_cli(),
    }
}
