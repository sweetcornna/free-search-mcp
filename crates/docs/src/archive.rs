//! Archive LISTING, never extraction (port of `_parse_archive`). A zip bomb
//! is small on disk and enormous expanded, so ZIPs are listed from the
//! central directory and tarballs are walked as a stream whose decompressed
//! volume is capped.

use crate::markdown::{human_bytes, rows_to_markdown};
use crate::office::MAX_ARCHIVE_ENTRIES;
use std::cell::Cell;
use std::io::{Cursor, Read};

/// Stops a decompressor after `cap` bytes, flagging `hit`.
struct ScanCap<'a, R> {
    inner: R,
    left: u64,
    hit: &'a Cell<bool>,
}

impl<R: Read> Read for ScanCap<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            self.hit.set(true);
            return Err(std::io::Error::other("archive scan limit reached"));
        }
        let want = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        self.left -= n as u64;
        Ok(n)
    }
}

/// `_parse_archive(blob, source)` → `(text, truncated)`. `scan_cap` bounds
/// the decompressed bytes walked for a compressed tarball.
pub(crate) fn parse_archive(blob: &[u8], source: &str, scan_cap: u64) -> (String, bool) {
    let mut rows: Vec<Vec<String>> = vec![vec!["path".into(), "size".into(), "compressed".into()]];
    let mut total_declared = 0u64;
    let mut truncated = false;
    let mut scan_stopped = false;
    let mut add = |path: String, size: u64, comp: u64, rows: &mut Vec<Vec<String>>| -> bool {
        if rows.len() > MAX_ARCHIVE_ENTRIES {
            truncated = true;
            return false;
        }
        let path: String = if path.chars().count() > 4096 {
            path.chars().take(4096).collect()
        } else {
            path
        };
        rows.push(vec![path, human_bytes(size), human_bytes(comp)]);
        total_declared = total_declared.saturating_add(size);
        true
    };

    if let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(blob)) {
        for i in 0..zip.len() {
            let entry = match zip.by_index_raw(i) {
                Ok(e) => e,
                Err(e) => return (format!("Could not read archive: {e}"), false),
            };
            let name = entry
                .name()
                .map(|n| n.into_owned())
                .unwrap_or_else(|_| format!("<entry {i}>"));
            let (size, comp) = (entry.size(), entry.compressed_size());
            drop(entry);
            if !add(name, size, comp, &mut rows) {
                break;
            }
        }
    } else {
        let hit = Cell::new(false);
        let raw: Box<dyn Read + '_> = if blob.starts_with(&[0x1f, 0x8b]) {
            Box::new(flate2::read::MultiGzDecoder::new(blob))
        } else if blob.starts_with(b"BZh") {
            Box::new(bzip2::read::MultiBzDecoder::new(blob))
        } else if blob.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
            Box::new(liblzma::read::XzDecoder::new_multi_decoder(blob))
        } else if blob.len() >= 512 && blob.get(257..262) == Some(b"ustar")
            || looks_like_v7_tar(blob)
        {
            Box::new(blob)
        } else {
            return (
                "Could not read archive: not a ZIP or tar archive".to_owned(),
                false,
            );
        };
        let mut tar = tar::Archive::new(ScanCap {
            inner: raw,
            left: scan_cap,
            hit: &hit,
        });
        let entries = match tar.entries() {
            Ok(e) => e,
            Err(e) => return (format!("Could not read archive: {e}"), false),
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) if hit.get() => {
                    scan_stopped = true;
                    break;
                }
                Err(e) => return (format!("Could not read archive: {e}"), false),
            };
            let header = entry.header();
            if !matches!(
                header.entry_type(),
                tar::EntryType::Regular | tar::EntryType::Continuous
            ) {
                continue;
            }
            let size = header.size().unwrap_or(0);
            let name = entry
                .path()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !add(name, size, size, &mut rows) {
                break;
            }
        }
    }

    let files = rows.len() - 1;
    let ratio = total_declared as f64 / blob.len().max(1) as f64;
    let mut header = format!(
        "Archive listing for `{source}` — {files} files, {} uncompressed ({ratio:.0}x compression).",
        human_bytes(total_declared)
    );
    if ratio > 100.0 {
        header.push_str(
            "\n\n> This expands more than 100x. Treat it as untrusted and do not extract it \
             without checking what is inside.",
        );
    }
    if scan_stopped {
        header.push_str(&format!(
            "\n\n> Listing stopped after scanning {} of decompressed data.",
            human_bytes(scan_cap)
        ));
    }
    header.push_str("\n\n> Contents are listed, not extracted.");
    (
        format!("{header}\n\n{}", rows_to_markdown(&rows, 30)),
        truncated || scan_stopped,
    )
}

/// A pre-POSIX tar header: valid octal checksum over the first block.
fn looks_like_v7_tar(blob: &[u8]) -> bool {
    if blob.len() < 512 {
        return false;
    }
    let block = &blob[..512];
    let field = &block[148..156];
    let digits: String = field
        .iter()
        .take_while(|b| (b'0'..=b'7').contains(b) || **b == b' ')
        .map(|b| *b as char)
        .collect();
    let Ok(want) = u32::from_str_radix(digits.trim(), 8) else {
        return false;
    };
    let sum: u32 = block
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if (148..156).contains(&i) {
                32
            } else {
                u32::from(*b)
            }
        })
        .sum();
    sum == want && block[0] != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    pub(crate) fn zip_bytes(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn tar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, name, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn test_zip_is_listed_not_extracted() {
        let blob = zip_bytes(&[("a.txt", vec![b'x'; 100]), ("dir/b.txt", vec![b'y'; 50])]);
        let (text, _) = parse_archive(&blob, "a.zip", 1 << 30);
        assert!(text.contains("a.txt") && text.contains("dir/b.txt"));
        assert!(text.contains("listed, not extracted"));
        assert!(!text.contains("xxxxx"));
    }

    #[test]
    fn test_tar_is_listed_too() {
        let (text, _) = parse_archive(&tar_bytes(&[("inside.txt", &[b'z'; 40])]), "a.tar", 1 << 30);
        assert!(text.contains("inside.txt"), "{text}");
    }

    #[test]
    fn compressed_tarballs_are_listed() {
        let tar = tar_bytes(&[("one.txt", b"1"), ("two/three.txt", b"333")]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar).unwrap();
        let gz = gz.finish().unwrap();
        let mut bz = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
        bz.write_all(&tar).unwrap();
        let bz = bz.finish().unwrap();
        let mut xz = liblzma::write::XzEncoder::new(Vec::new(), 6);
        xz.write_all(&tar).unwrap();
        let xz = xz.finish().unwrap();
        for (blob, name) in [(gz, "a.tgz"), (bz, "a.tar.bz2"), (xz, "a.tar.xz")] {
            let (text, truncated) = parse_archive(&blob, name, 1 << 30);
            assert!(
                text.contains("one.txt") && text.contains("two/three.txt"),
                "{name}: {text}"
            );
            assert!(!truncated, "{name}");
        }
    }

    #[test]
    fn test_zip_bomb_ratio_is_called_out() {
        let blob = zip_bytes(&[("bomb.txt", vec![0u8; 2_000_000])]);
        let (text, _) = parse_archive(&blob, "bomb.zip", 1 << 30);
        assert!(text.contains("expands more than 100x"));
    }

    #[test]
    fn test_archive_entry_count_is_capped() {
        let entries: Vec<(String, Vec<u8>)> = (0..2100)
            .map(|i| (format!("f{i}.txt"), vec![b'x']))
            .collect();
        let refs: Vec<(&str, Vec<u8>)> = entries
            .iter()
            .map(|(n, d)| (n.as_str(), d.clone()))
            .collect();
        let (text, truncated) = parse_archive(&zip_bytes(&refs), "many.zip", 1 << 30);
        assert!(truncated);
        assert!(text.contains("— 2000 files"), "{}", &text[..200]);
        let exact: Vec<(String, Vec<u8>)> = (0..2000).map(|i| (format!("f{i}"), vec![])).collect();
        let refs: Vec<(&str, Vec<u8>)> =
            exact.iter().map(|(n, d)| (n.as_str(), d.clone())).collect();
        assert!(!parse_archive(&zip_bytes(&refs), "exact.zip", 1 << 30).1);
    }

    #[test]
    fn test_corrupt_archive_reports_instead_of_raising() {
        let (text, truncated) = parse_archive(b"not an archive at all", "broken.zip", 1 << 30);
        assert!(text.contains("Could not read archive"));
        assert!(!truncated);
        let (text, _) = parse_archive(&[0x1f, 0x8b, 0, 1, 2, 3], "bad.tgz", 1 << 30);
        assert!(text.contains("Could not read archive"), "{text}");
    }

    #[test]
    fn gzip_bomb_tarball_listing_stops_at_the_scan_cap() {
        let tar = tar_bytes(&[("big.bin", &vec![0u8; 4 << 20]), ("after.txt", b"x")]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(&tar).unwrap();
        let gz = gz.finish().unwrap();
        let (text, truncated) = parse_archive(&gz, "bomb.tgz", 1 << 20);
        assert!(truncated);
        assert!(text.contains("Listing stopped"), "{text}");
        assert!(!text.contains("after.txt"));
    }
}
