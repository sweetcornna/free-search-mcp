//! A small in-process cache of parsed documents, so paginating a document
//! (`doc.read` with increasing `start`) or asking for its outline does not
//! re-download and re-parse it each time.
//!
//! Invariants (tested in `tests/documents.rs`): a local hit is only served
//! after the caller has already set the local-doc taint; a remote hit is
//! only served after `check_url_offline`; a local key includes the file's
//! size and mtime, so an edited file is re-read.

use crate::parsed::ParsedDoc;
use crate::sandbox::FileStamp;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum CacheKey {
    Remote(String),
    Local { path: PathBuf, stamp: FileStamp },
}

struct Entry {
    key: CacheKey,
    doc: Arc<ParsedDoc>,
    at: Instant,
}

pub(crate) struct ParsedCache {
    entries: Mutex<VecDeque<Entry>>,
    max_entries: usize,
    max_bytes: usize,
    remote_ttl: Duration,
}

impl ParsedCache {
    pub(crate) fn new(max_entries: usize, max_bytes: usize, remote_ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
            max_entries,
            max_bytes,
            remote_ttl,
        }
    }

    pub(crate) fn get(&self, key: &CacheKey) -> Option<Arc<ParsedDoc>> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pos = entries.iter().position(|e| &e.key == key)?;
        let fresh = match key {
            CacheKey::Remote(_) => entries[pos].at.elapsed() < self.remote_ttl,
            CacheKey::Local { .. } => true,
        };
        if !fresh {
            entries.remove(pos);
            return None;
        }
        // Most recently used at the back.
        let e = entries.remove(pos)?;
        let doc = Arc::clone(&e.doc);
        entries.push_back(e);
        Some(doc)
    }

    pub(crate) fn put(&self, key: CacheKey, doc: Arc<ParsedDoc>) {
        if doc.approx_bytes() > self.max_bytes {
            return;
        }
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|e| e.key != key);
        // A newer stamp of the same local file replaces the old one.
        if let CacheKey::Local { path, .. } = &key {
            entries.retain(|e| !matches!(&e.key, CacheKey::Local { path: p, .. } if p == path));
        }
        entries.push_back(Entry {
            key,
            doc,
            at: Instant::now(),
        });
        let mut total: usize = entries.iter().map(|e| e.doc.approx_bytes()).sum();
        while entries.len() > self.max_entries || total > self.max_bytes {
            let Some(old) = entries.pop_front() else {
                break;
            };
            total -= old.doc.approx_bytes();
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.lock().map_or(0, |e| e.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::DocFormat;

    fn doc(text: &str) -> Arc<ParsedDoc> {
        Arc::new(ParsedDoc {
            format: DocFormat::Text,
            title: None,
            text: text.to_owned(),
            total_chars: text.chars().count(),
            pages: None,
            truncated: false,
            sections: Vec::new(),
            sheets: None,
        })
    }

    #[test]
    fn lru_bounds_entries_and_bytes_and_expires_remote() {
        let c = ParsedCache::new(2, 100, Duration::from_millis(30));
        c.put(CacheKey::Remote("a".into()), doc("aaa"));
        c.put(CacheKey::Remote("b".into()), doc("bbb"));
        assert!(c.get(&CacheKey::Remote("a".into())).is_some()); // a is now most recent
        c.put(CacheKey::Remote("c".into()), doc("ccc"));
        assert!(
            c.get(&CacheKey::Remote("b".into())).is_none(),
            "LRU evicts b"
        );
        assert_eq!(c.len(), 2);
        c.put(CacheKey::Remote("big".into()), doc(&"x".repeat(200)));
        assert!(
            c.get(&CacheKey::Remote("big".into())).is_none(),
            "oversized docs are not cached"
        );
        std::thread::sleep(Duration::from_millis(40));
        assert!(
            c.get(&CacheKey::Remote("a".into())).is_none(),
            "remote entries expire"
        );
    }

    #[test]
    fn local_keys_include_the_file_stamp() {
        let c = ParsedCache::new(4, 1000, Duration::from_secs(60));
        let k = |len| CacheKey::Local {
            path: PathBuf::from("/r/a.txt"),
            stamp: FileStamp {
                len,
                modified: None,
            },
        };
        c.put(k(3), doc("old"));
        assert!(c.get(&k(4)).is_none());
        c.put(k(4), doc("newer"));
        assert_eq!(c.len(), 1, "a newer stamp replaces the old entry");
        assert_eq!(c.get(&k(4)).unwrap().text, "newer");
    }
}
