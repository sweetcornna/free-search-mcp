//! Wire protocol between the server and the isolated `__pdf-worker` process.
//!
//! stdin: one JSON [`WorkerRequest`] line, then the raw PDF bytes until EOF.
//!
//! stdout: newline-separated messages `FSM-PDF/1 <kind> <json>`. Anything
//! else on stdout (a library's stray print) is ignored. Kinds, in order:
//! `meta` (once), `page` (per non-empty page, streamed as extracted, so a
//! worker killed half-way still yields the pages before the hostile one),
//! `done` (once), then optionally `outline`. A failure is a single `error`.

use serde::{Deserialize, Serialize};

/// Prefix of every protocol line.
pub const LINE_PREFIX: &str = "FSM-PDF/1 ";
/// The hidden subcommand the server binary dispatches to
/// [`crate::DocService::run_pdf_worker_cli`].
pub const PDF_WORKER_SUBCOMMAND: &str = "__pdf-worker";

/// Exit codes of the worker.
pub mod exit {
    pub const OK: u8 = 0;
    pub const FAILED: u8 = 2;
    pub const PANIC: u8 = 3;
    pub const BAD_REQUEST: u8 = 4;
}

/// The header line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRequest {
    pub v: u32,
    pub max_pages: u32,
    pub max_chars: usize,
    pub max_bytes: u64,
    pub outline: bool,
    /// `RLIMIT_DATA` the worker applies to itself (best effort), MiB.
    #[serde(default)]
    pub rlimit_data_mb: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetaMsg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub total_pages: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageMsg {
    /// 1-based page number.
    pub n: u32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoneMsg {
    pub truncated: bool,
    pub page_errors: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutlineItemMsg {
    pub title: String,
    pub level: u8,
    /// 0-based page index, when the bookmark resolves to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutlineMsg {
    pub items: Vec<OutlineItemMsg>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorMsg {
    /// `parse` | `encrypted` | `input` | `request` | `panic`.
    pub kind: String,
    pub error: String,
}

/// One parsed protocol line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerMsg {
    Meta(MetaMsg),
    Page(PageMsg),
    Done(DoneMsg),
    Outline(OutlineMsg),
    Error(ErrorMsg),
}

impl WorkerMsg {
    /// `FSM-PDF/1 <kind> <json>` (no trailing newline).
    pub fn to_line(&self) -> String {
        let (kind, json) = match self {
            Self::Meta(m) => ("meta", serde_json::to_string(m)),
            Self::Page(m) => ("page", serde_json::to_string(m)),
            Self::Done(m) => ("done", serde_json::to_string(m)),
            Self::Outline(m) => ("outline", serde_json::to_string(m)),
            Self::Error(m) => ("error", serde_json::to_string(m)),
        };
        format!(
            "{LINE_PREFIX}{kind} {}",
            json.unwrap_or_else(|_| "{}".to_owned())
        )
    }

    /// Parse one stdout line; `None` for noise or a malformed message.
    pub fn parse_line(line: &[u8]) -> Option<Self> {
        let line = std::str::from_utf8(line)
            .ok()?
            .trim_end_matches(['\r', '\n']);
        let rest = line.strip_prefix(LINE_PREFIX)?;
        let (kind, json) = rest.split_once(' ')?;
        Some(match kind {
            "meta" => Self::Meta(serde_json::from_str(json).ok()?),
            "page" => Self::Page(serde_json::from_str(json).ok()?),
            "done" => Self::Done(serde_json::from_str(json).ok()?),
            "outline" => Self::Outline(serde_json::from_str(json).ok()?),
            "error" => Self::Error(serde_json::from_str(json).ok()?),
            _ => return None,
        })
    }
}

/// Everything a worker run said, collected from its stdout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerTranscript {
    pub meta: Option<MetaMsg>,
    pub pages: Vec<PageMsg>,
    pub done: Option<DoneMsg>,
    pub outline: Option<OutlineMsg>,
    pub error: Option<ErrorMsg>,
    /// Protocol lines seen (noise excluded).
    pub lines: usize,
}

impl WorkerTranscript {
    pub fn from_stdout(stdout: &[u8]) -> Self {
        let mut t = Self::default();
        for line in stdout.split(|b| *b == b'\n') {
            let Some(msg) = WorkerMsg::parse_line(line) else {
                continue;
            };
            t.lines += 1;
            match msg {
                WorkerMsg::Meta(m) => t.meta = Some(m),
                WorkerMsg::Page(p) => t.pages.push(p),
                WorkerMsg::Done(d) => t.done = Some(d),
                WorkerMsg::Outline(o) => t.outline = Some(o),
                WorkerMsg::Error(e) => t.error = Some(e),
            }
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_round_trip_and_noise_is_ignored() {
        let msgs = [
            WorkerMsg::Meta(MetaMsg {
                title: Some("T\nx".into()),
                total_pages: 3,
            }),
            WorkerMsg::Page(PageMsg {
                n: 1,
                text: "a\nb \u{1}".into(),
            }),
            WorkerMsg::Done(DoneMsg {
                truncated: false,
                page_errors: 0,
                first_error: None,
            }),
            WorkerMsg::Outline(OutlineMsg {
                items: vec![OutlineItemMsg {
                    title: "I".into(),
                    level: 1,
                    page: Some(0),
                }],
            }),
        ];
        let mut out = String::from("stray library print\n");
        for m in &msgs {
            let line = m.to_line();
            assert!(!line.contains('\n'), "one message per line");
            assert_eq!(WorkerMsg::parse_line(line.as_bytes()).as_ref(), Some(m));
            out.push_str(&line);
            out.push('\n');
        }
        out.push_str("FSM-PDF/1 page {broken\nFSM-PDF/1 bogus {}\n");
        let t = WorkerTranscript::from_stdout(out.as_bytes());
        assert_eq!(t.lines, 4);
        assert_eq!(t.pages.len(), 1);
        assert!(t.done.is_some() && t.outline.is_some() && t.error.is_none());
    }
}
