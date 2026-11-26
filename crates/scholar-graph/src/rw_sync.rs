//! 1.0 Retraction Watch lazy sync (`SPEC.md` §6, §8, I15).
//!
//! Triggered in the background on the first `graph()` or `integrity()` call
//! (when `cfg.offline_sync` is true) with its own detached deadline and
//! cancellation token so slow downloads never starve the caller's budget.
//! Acquires a non-blocking cross-process `fd-lock` on `<cache_dir>/rw-sync.lock`,
//! checks `sync_state` for 7-day freshness / 1-hour retry backoff / `ETag`,
//! downloads the CC0 Retraction Watch CSV (rejecting truncated responses),
//! parses it record-by-record through `csv::Reader` inside `spawn_blocking`
//! (without writing a temporary raw CSV copy to disk, since frozen `NetClient`
//! buffers `HttpBytes.body` in memory), atomically replaces rows via
//! `StoreApi::retractions_replace_batch`, and records completion in `sync_state`.
//!
//! Concurrent processes (or tasks) that cannot immediately acquire the lock skip
//! syncing without blocking and rely on Crossref's online fallback.

use crate::ids::{is_retraction_nature, normalize_doi, notice_rank};
use fsm_core::{
    CallCx, FsmError, HttpRequest, NetCx, RetractionNotice, SourceDeps, TransportProfile,
};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Official Crossref/Retraction Watch CC0 CSV URL (~70 MB).
pub const RW_CSV_URL: &str =
    "https://gitlab.com/crossref/retraction-watch-data/-/raw/main/retraction_watch.csv";

/// `sync_state.name` key for Retraction Watch sync.
pub const RW_SYNC_NAME: &str = "retraction_watch";

/// Lock file name inside `<cache_dir>`.
pub const RW_LOCK_FILE: &str = "rw-sync.lock";

/// Refresh cadence: 7 days.
pub const RW_REFRESH_INTERVAL_S: i64 = 7 * 24 * 60 * 60;

/// Minimum wait before retrying an unfinished or failed sync attempt (1 hour).
pub const RW_RETRY_BACKOFF_S: i64 = 3600;

/// Maximum CSV response size (150 MiB, safely above the ~70 MB CSV and overriding
/// `Config.max_response_bytes`'s 25 MB default per `w0a-review.md` Note #3).
pub const RW_MAX_CSV_BYTES: u64 = 150 * 1024 * 1024;

/// Maximum time spent on a single detached Retraction Watch sync transfer (10 min).
pub const RW_SYNC_TIMEOUT: Duration = Duration::from_secs(600);

/// Outcome of a lazy Retraction Watch sync attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Disabled via `SEARCH_MCP_OFFLINE_SYNC=false`.
    Disabled,
    /// Already synced within the last 7 days.
    Fresh,
    /// Upstream returned `304 Not Modified` for the cached ETag; freshness renewed.
    NotModified,
    /// Another process/task holds `rw-sync.lock`, or a recent attempt is in its 1h backoff window.
    SkippedBusy,
    /// Downloaded, parsed, and indexed the Retraction Watch CSV.
    Synced { rows: usize, etag: Option<String> },
    /// Non-fatal sync failure (network/HTTP/truncation/parse error); caller uses online fallback.
    Failed(String),
}

fn now_unix_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Ensure the local Retraction Watch table is populated and at most 7 days old.
/// Never fails the caller on transient network/lock errors; returns a typed
/// [`SyncOutcome`] so callers can seamlessly fall back to Crossref online.
pub async fn ensure_synced(
    deps: &SourceDeps,
    in_proc_lock: &Arc<tokio::sync::Mutex<()>>,
    cx: &CallCx,
) -> SyncOutcome {
    if !deps.cfg.offline_sync {
        return SyncOutcome::Disabled;
    }
    if cx.cancel.is_cancelled() {
        return SyncOutcome::Failed("cancelled".into());
    }

    let now = now_unix_s();
    // Fast path: check freshness and failure/in-progress backoff before touching locks.
    match deps.store.sync_state_get(RW_SYNC_NAME).await {
        Ok(Some((_, _, Some(finished_at))))
            if now.saturating_sub(finished_at) < RW_REFRESH_INTERVAL_S =>
        {
            return SyncOutcome::Fresh;
        }
        Ok(Some((_, started_at, None))) if now.saturating_sub(started_at) < RW_RETRY_BACKOFF_S => {
            return SyncOutcome::SkippedBusy;
        }
        _ => {}
    }

    // Intra-process non-blocking guard (complements cross-process fd-lock).
    let Ok(in_proc_guard) = in_proc_lock.clone().try_lock_owned() else {
        return SyncOutcome::SkippedBusy;
    };

    ensure_synced_with_guard(deps, in_proc_guard, cx).await
}

/// Core sync routine when the caller has already acquired the intra-process lock
/// (used by detached background spawns to eliminate spawn-window races).
pub(crate) async fn ensure_synced_with_guard(
    deps: &SourceDeps,
    _in_proc_guard: tokio::sync::OwnedMutexGuard<()>,
    cx: &CallCx,
) -> SyncOutcome {
    if !deps.cfg.offline_sync {
        return SyncOutcome::Disabled;
    }
    if cx.cancel.is_cancelled() {
        return SyncOutcome::Failed("cancelled".into());
    }

    let now = now_unix_s();
    match deps.store.sync_state_get(RW_SYNC_NAME).await {
        Ok(Some((_, _, Some(finished_at))))
            if now.saturating_sub(finished_at) < RW_REFRESH_INTERVAL_S =>
        {
            return SyncOutcome::Fresh;
        }
        Ok(Some((_, started_at, None))) if now.saturating_sub(started_at) < RW_RETRY_BACKOFF_S => {
            return SyncOutcome::SkippedBusy;
        }
        _ => {}
    }

    // Ensure `<cache_dir>` exists and open `<cache_dir>/rw-sync.lock` on a blocking thread (M4).
    let cache_dir = deps.cfg.cache_dir.clone();
    let lock_path = cache_dir.join(RW_LOCK_FILE);
    let open_dir = cache_dir.clone();
    let open_path = lock_path.clone();
    let lock_file = match tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
        std::fs::create_dir_all(&open_dir)?;
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&open_path)
    })
    .await
    {
        Ok(Ok(f)) => f,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, path = %lock_path.display(), "rw-sync: cannot open lock file");
            return SyncOutcome::Failed(format!("open lock file: {e}"));
        }
        Err(e) => {
            tracing::warn!(error = %e, "rw-sync: lock open task failed");
            return SyncOutcome::Failed(format!("open lock join: {e}"));
        }
    };

    let mut fd_lock = fd_lock::RwLock::new(lock_file);
    let _fd_guard = match fd_lock.try_write() {
        Ok(guard) => guard,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            tracing::debug!(path = %lock_path.display(), "rw-sync: lock held by another process; skipping");
            return SyncOutcome::SkippedBusy;
        }
        Err(e) => {
            tracing::warn!(error = %e, path = %lock_path.display(), "rw-sync: lock error");
            return SyncOutcome::Failed(format!("lock error: {e}"));
        }
    };

    // Double-check freshness & retry backoff after acquiring the lock.
    let state = match deps.store.sync_state_get(RW_SYNC_NAME).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "rw-sync: sync_state_get failed");
            return SyncOutcome::Failed(format!("sync_state_get: {e}"));
        }
    };
    let now = now_unix_s();
    let existing_etag = match state {
        Some((_, _, Some(finished_at)))
            if now.saturating_sub(finished_at) < RW_REFRESH_INTERVAL_S =>
        {
            return SyncOutcome::Fresh;
        }
        Some((_, started_at, None)) if now.saturating_sub(started_at) < RW_RETRY_BACKOFF_S => {
            return SyncOutcome::SkippedBusy;
        }
        Some((etag, _, _)) => etag,
        None => None,
    };

    let started_at = now;
    if let Err(e) = deps
        .store
        .sync_state_mark(RW_SYNC_NAME, existing_etag.as_deref(), started_at, None)
        .await
    {
        tracing::warn!(error = %e, "rw-sync: sync_state_mark(start) failed");
    }

    let mut req = HttpRequest::get(RW_CSV_URL)
        .header("Accept", "text/csv, text/plain;q=0.9, */*;q=0.5")
        .for_source(RW_SYNC_NAME)
        .with_max_bytes(RW_MAX_CSV_BYTES);
    if let Some(ref etag) = existing_etag {
        req = req.header("If-None-Match", etag.clone());
    }

    let timeout = cx
        .remaining()
        .min(RW_SYNC_TIMEOUT)
        .max(Duration::from_secs(1));
    let ncx = NetCx::new(cx.cancel.child_token(), timeout);
    let resp = match deps.net.send(req, TransportProfile::Plain, &ncx).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "rw-sync: CSV download failed");
            return SyncOutcome::Failed(e.to_string());
        }
    };

    if resp.status == 304 {
        let finished_at = now_unix_s();
        if let Err(e) = deps
            .store
            .sync_state_mark(
                RW_SYNC_NAME,
                existing_etag.as_deref(),
                started_at,
                Some(finished_at),
            )
            .await
        {
            tracing::warn!(error = %e, "rw-sync: sync_state_mark(304) failed");
            return SyncOutcome::Failed(e.to_string());
        }
        return SyncOutcome::NotModified;
    }

    if !resp.is_success() {
        let msg = format!("unexpected HTTP {} from Retraction Watch CSV", resp.status);
        tracing::warn!("{msg}");
        return SyncOutcome::Failed(msg);
    }

    if resp.truncated {
        let msg = format!(
            "Retraction Watch CSV exceeded the {} MiB cap and was truncated",
            RW_MAX_CSV_BYTES / (1024 * 1024)
        );
        tracing::warn!("{msg}");
        return SyncOutcome::Failed(msg);
    }

    let new_etag = resp.header("etag").map(|s| s.trim().to_owned());
    let body = resp.body;

    // Parse directly from the in-memory `Bytes` buffer via `csv::Reader` inside
    // `spawn_blocking` (no temporary CSV copy written to disk).
    let parsed = tokio::task::spawn_blocking(move || parse_retraction_watch_csv(body.as_ref()))
        .await
        .map_err(|e| FsmError::internal("rw_sync_join", e.to_string()))
        .and_then(|res| res);

    let rows = match parsed {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "rw-sync: CSV parse failed");
            return SyncOutcome::Failed(e.to_string());
        }
    };

    // M2: Never wipe an existing local table with an empty / zero-DOI upstream export.
    if rows.is_empty() {
        let msg = "Retraction Watch CSV contained zero valid DOI rows".to_owned();
        tracing::warn!("{msg}");
        return SyncOutcome::Failed(msg);
    }

    let count = rows.len();
    if let Err(e) = deps
        .store
        .retractions_replace_batch(rows, new_etag.clone())
        .await
    {
        tracing::warn!(error = %e, "rw-sync: retractions_replace_batch failed");
        return SyncOutcome::Failed(e.to_string());
    }

    let finished_at = now_unix_s();
    if let Err(e) = deps
        .store
        .sync_state_mark(
            RW_SYNC_NAME,
            new_etag.as_deref(),
            started_at,
            Some(finished_at),
        )
        .await
    {
        tracing::warn!(error = %e, "rw-sync: sync_state_mark(finish) failed");
        return SyncOutcome::Failed(e.to_string());
    }

    SyncOutcome::Synced {
        rows: count,
        etag: new_etag,
    }
}

#[derive(Default)]
struct DoiNoticeAccumulator {
    best_non_reinst: Option<RetractionNotice>,
    has_retraction: bool,
    latest_retraction_date: Option<String>,
    latest_reinstatement: Option<RetractionNotice>,
}

fn is_reinstatement_nature(kind: &str) -> bool {
    kind.trim().to_ascii_lowercase().contains("reinstat")
}

/// Stream-parse a Retraction Watch CSV reader into deduplicated
/// `(normalized_original_doi, RetractionNotice)` rows.
pub fn parse_retraction_watch_csv<R: std::io::Read>(
    reader: R,
) -> Result<Vec<(String, RetractionNotice)>, FsmError> {
    let mut csv_reader = csv::ReaderBuilder::new()
        .flexible(true)
        .trim(csv::Trim::Headers)
        .from_reader(reader);

    let headers = csv_reader
        .headers()
        .map_err(|e| FsmError::upstream(RW_SYNC_NAME, None, format!("invalid CSV headers: {e}")))?
        .clone();

    let col = |wanted: &str| -> Option<usize> {
        headers.iter().position(|h| {
            h.trim()
                .trim_start_matches('\u{feff}')
                .eq_ignore_ascii_case(wanted)
        })
    };

    let idx_orig_doi = col("OriginalPaperDOI").ok_or_else(|| {
        FsmError::upstream(
            RW_SYNC_NAME,
            None,
            "Retraction Watch CSV missing OriginalPaperDOI header",
        )
    })?;
    let idx_retr_doi = col("RetractionDOI");
    let idx_nature = col("RetractionNature");
    let idx_date = col("RetractionDate");
    let idx_reason = col("Reason");

    let mut dedup: HashMap<String, DoiNoticeAccumulator> = HashMap::new();
    let mut record = csv::StringRecord::new();

    while csv_reader
        .read_record(&mut record)
        .map_err(|e| FsmError::upstream(RW_SYNC_NAME, None, format!("CSV read error: {e}")))?
    {
        let raw_orig_doi = record.get(idx_orig_doi).unwrap_or("");
        let Some(orig_doi) = normalize_doi(raw_orig_doi) else {
            continue;
        };

        let raw_nature = idx_nature
            .and_then(|i| record.get(i))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Retraction");

        let notice_doi = idx_retr_doi
            .and_then(|i| record.get(i))
            .and_then(normalize_doi);

        let date = idx_date
            .and_then(|i| record.get(i))
            .and_then(normalize_rw_date);

        let reason = idx_reason
            .and_then(|i| record.get(i))
            .and_then(clean_rw_reason);

        let mut notice = RetractionNotice::new(raw_nature, RW_SYNC_NAME);
        notice.date = date;
        notice.reason = reason;
        notice.notice_doi = notice_doi;

        let acc = dedup.entry(orig_doi).or_default();
        if is_reinstatement_nature(&notice.nature) {
            let replace = match &acc.latest_reinstatement {
                None => true,
                Some(cur) => {
                    notice.date.as_deref().unwrap_or("") >= cur.date.as_deref().unwrap_or("")
                }
            };
            if replace {
                acc.latest_reinstatement = Some(notice);
            }
            continue;
        }

        if is_retraction_nature(&notice.nature) {
            acc.has_retraction = true;
            if let Some(ref d) = notice.date
                && acc
                    .latest_retraction_date
                    .as_deref()
                    .is_none_or(|cur| d.as_str() > cur)
            {
                acc.latest_retraction_date = Some(d.clone());
            }
        }

        match acc.best_non_reinst.as_mut() {
            None => {
                acc.best_non_reinst = Some(notice);
            }
            Some(existing) => {
                let new_rank = notice_rank(&notice.nature);
                let cur_rank = notice_rank(&existing.nature);
                if new_rank < cur_rank
                    || (new_rank == cur_rank
                        && notice.date.as_deref().unwrap_or("")
                            > existing.date.as_deref().unwrap_or(""))
                {
                    if notice.reason.is_none() {
                        notice.reason = existing.reason.clone();
                    }
                    if notice.notice_doi.is_none() {
                        notice.notice_doi = existing.notice_doi.clone();
                    }
                    *existing = notice;
                } else {
                    if existing.reason.is_none() {
                        existing.reason = notice.reason;
                    }
                    if existing.notice_doi.is_none() {
                        existing.notice_doi = notice.notice_doi;
                    }
                }
            }
        }
    }

    let mut rows: Vec<(String, RetractionNotice)> = dedup
        .into_iter()
        .filter_map(|(doi, acc)| {
            let chosen = match (acc.best_non_reinst, acc.latest_reinstatement) {
                (None, Some(reinst)) => reinst,
                (Some(best), None) => best,
                (Some(best), Some(reinst)) => {
                    // A dated Reinstatement strictly later than the latest retraction
                    // supersedes the retraction so reinstated papers are not flagged retracted.
                    let reinst_wins = if acc.has_retraction {
                        match (
                            reinst.date.as_deref(),
                            acc.latest_retraction_date.as_deref(),
                        ) {
                            (Some(rd), Some(retd)) => rd > retd,
                            (Some(_), None) => true,
                            (None, _) => false,
                        }
                    } else {
                        reinst.date.as_deref().unwrap_or("") > best.date.as_deref().unwrap_or("")
                    };
                    if reinst_wins { reinst } else { best }
                }
                (None, None) => return None,
            };
            Some((doi, chosen))
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(rows)
}

/// Normalize Retraction Watch date values such as `"6/5/2020 0:00"`, `"06/05/2020"`,
/// or ISO `"2020-06-05"` into canonical `YYYY-MM-DD` / `YYYY-MM` / `YYYY`.
pub fn normalize_rw_date(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(iso) = fsm_core::dates::normalize_date(trimmed) {
        return Some(iso);
    }
    // Retraction Watch CSV frequently uses US `M/D/YYYY` with optional `H:MM[:SS]`.
    let date_part = trimmed.split_whitespace().next()?;
    let parts: Vec<&str> = date_part.split('/').collect();
    if parts.len() == 3 {
        let m: i8 = parts[0].parse().ok()?;
        let d: i8 = parts[1].parse().ok()?;
        let y: i16 = parts[2].parse().ok()?;
        if let Ok(dt) = jiff::civil::Date::new(y, m, d) {
            return Some(dt.to_string());
        }
    }
    None
}

/// Clean Retraction Watch's `Reason` syntax (`"+Falsification;+Error in Data;"` ->
/// `"Falsification; Error in Data"`).
pub fn clean_rw_reason(raw: &str) -> Option<String> {
    let parts: Vec<&str> = raw
        .split(';')
        .map(|s| s.trim().trim_start_matches('+').trim())
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rw_date_and_reason_normalization() {
        assert_eq!(
            normalize_rw_date("6/5/2020 0:00").as_deref(),
            Some("2020-06-05")
        );
        assert_eq!(
            normalize_rw_date("12/31/2023").as_deref(),
            Some("2023-12-31")
        );
        assert_eq!(
            normalize_rw_date("2020-06-05").as_deref(),
            Some("2020-06-05")
        );
        assert_eq!(normalize_rw_date("unavailable"), None);
        assert_eq!(
            clean_rw_reason("+Falsification/Fabrication of Data;+Investigation by Journal;")
                .as_deref(),
            Some("Falsification/Fabrication of Data; Investigation by Journal")
        );
        assert_eq!(clean_rw_reason(" ; + ; "), None);
    }
}
