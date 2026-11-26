//! `free-search-mcp` binary (`SPEC.md` §2, §7.3, §9.1; lane `W6a`).
//!
//! Short-circuits the hidden `__pdf-worker` subcommand BEFORE initializing Tokio,
//! logging, or any stdout I/O, because the isolated PDF worker owns stdout.

use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    // W4b contract: run_pdf_worker_cli() MUST run before tokio/logging/stdout setup.
    if std::env::args_os()
        .nth(1)
        .and_then(|arg| arg.into_string().ok())
        .as_deref()
        == Some(fsm_docs::PDF_WORKER_SUBCOMMAND)
    {
        return fsm_docs::DocService::run_pdf_worker_cli();
    }

    let cli = fsm_server::cli::Cli::parse();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("free-search-mcp: failed to start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(fsm_server::cli::run_cli(cli))
}
