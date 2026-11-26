//! Format detection (port of `documents.py` `_detect_format`, `_ext`,
//! `_structured_text_lang`), plus magic-byte sniffing for bodies whose name
//! and media type say nothing (`application/octet-stream` download links).

use std::fmt;

/// A document format, as reported in `DocSlice.format` / `DocOutline.format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DocFormat {
    Pdf,
    Docx,
    Xlsx,
    Pptx,
    Epub,
    Csv,
    Archive,
    Image,
    Html,
    Markdown,
    Code,
    Text,
    Unknown,
}

impl DocFormat {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Xlsx => "xlsx",
            Self::Pptx => "pptx",
            Self::Epub => "epub",
            Self::Csv => "csv",
            Self::Archive => "archive",
            Self::Image => "image",
            Self::Html => "html",
            Self::Markdown => "markdown",
            Self::Code => "code",
            Self::Text => "text",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for DocFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Source-code and config extensions read as plain text but fenced with the
/// language a Markdown renderer expects (not always the extension).
const CODE_LANGS: &[(&str, &str)] = &[
    ("py", "python"),
    ("pyi", "python"),
    ("js", "javascript"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("ts", "typescript"),
    ("tsx", "tsx"),
    ("jsx", "jsx"),
    ("rb", "ruby"),
    ("go", "go"),
    ("rs", "rust"),
    ("java", "java"),
    ("kt", "kotlin"),
    ("swift", "swift"),
    ("c", "c"),
    ("h", "c"),
    ("cc", "cpp"),
    ("cpp", "cpp"),
    ("hpp", "cpp"),
    ("cs", "csharp"),
    ("php", "php"),
    ("pl", "perl"),
    ("lua", "lua"),
    ("r", "r"),
    ("scala", "scala"),
    ("sh", "bash"),
    ("bash", "bash"),
    ("zsh", "bash"),
    ("fish", "fish"),
    ("ps1", "powershell"),
    ("sql", "sql"),
    ("graphql", "graphql"),
    ("proto", "protobuf"),
    ("tf", "hcl"),
    ("hcl", "hcl"),
    ("dockerfile", "dockerfile"),
    ("makefile", "makefile"),
    ("cmake", "cmake"),
    ("gradle", "groovy"),
    ("json", "json"),
    ("jsonl", "json"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
    ("toml", "toml"),
    ("ini", "ini"),
    ("cfg", "ini"),
    ("conf", "ini"),
    ("env", "bash"),
    ("xml", "xml"),
    ("svg", "xml"),
    ("css", "css"),
    ("scss", "scss"),
    ("less", "less"),
    ("vue", "vue"),
    ("svelte", "svelte"),
    ("tex", "latex"),
    ("diff", "diff"),
    ("patch", "diff"),
];

const ARCHIVE_EXTS: &[&str] = &[
    ".zip", ".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tar.xz", ".whl", ".jar",
];

const IMAGE_EXTS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".tiff", ".tif", ".ico", ".avif", ".heic",
    ".svg",
];

/// The fence language for an extension, if it is a known code/config type.
pub(crate) fn code_lang(ext: &str) -> Option<&'static str> {
    CODE_LANGS
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, lang)| *lang)
}

/// The path part of a source: the URL path for `scheme://` sources (so a
/// query string never decides the format), else the source itself.
pub(crate) fn source_path(source: &str) -> String {
    if !source.contains("://") {
        return source.to_owned();
    }
    match url::Url::parse(source) {
        Ok(u) => u.path().to_owned(),
        Err(_) => {
            let after_scheme = source.split_once("://").map_or(source, |(_, rest)| rest);
            let no_frag = after_scheme.split('#').next().unwrap_or("");
            let no_query = no_frag.split('?').next().unwrap_or("");
            no_query
                .find('/')
                .map_or_else(String::new, |i| no_query[i..].to_owned())
        }
    }
}

/// Lower-cased extension without the dot, ignoring any query string
/// (`_ext`). Extension-less build files keep a well-known identity.
pub(crate) fn ext(source: &str) -> String {
    let path = source_path(source);
    let name = path.rsplit(['/', '\\']).next().unwrap_or("").to_lowercase();
    if matches!(name.as_str(), "dockerfile" | "makefile" | "cmakelists.txt") {
        return name.split('.').next().unwrap_or("").to_owned();
    }
    match name.rfind('.') {
        Some(i) => name[i + 1..].to_owned(),
        None => String::new(),
    }
}

/// The fence language for a structured-text media type, else `""`
/// (`_structured_text_lang`).
pub(crate) fn structured_text_lang(ctype: &str) -> &'static str {
    let media = ctype
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if media.ends_with("/json") || media.ends_with("+json") || media.ends_with("/x-ndjson") {
        "json"
    } else if media.ends_with("/xml") || media.ends_with("+xml") {
        "xml"
    } else if media.ends_with("/yaml") || media.ends_with("/x-yaml") {
        "yaml"
    } else {
        ""
    }
}

/// `_detect_format`: classify by the path's extension, then the media type.
pub(crate) fn detect_format(source: &str, content_type: Option<&str>) -> DocFormat {
    let s = source_path(source).to_lowercase();
    let ctype = content_type.unwrap_or("").to_lowercase();
    let ends = |exts: &[&str]| exts.iter().any(|e| s.ends_with(e));
    if s.ends_with(".pdf") || ctype.contains("pdf") {
        DocFormat::Pdf
    } else if s.ends_with(".docx") || ctype.contains("wordprocessingml") {
        DocFormat::Docx
    } else if ends(&[".xlsx", ".xlsm"]) || ctype.contains("spreadsheetml") {
        DocFormat::Xlsx
    } else if s.ends_with(".pptx") || ctype.contains("presentationml") {
        DocFormat::Pptx
    } else if s.ends_with(".epub") || ctype.contains("epub") {
        DocFormat::Epub
    } else if ends(&[".csv", ".tsv"]) || ctype.contains("text/csv") {
        DocFormat::Csv
    } else if ends(ARCHIVE_EXTS) || ctype.contains("zip") || ctype.contains("x-tar") {
        DocFormat::Archive
    } else if ends(IMAGE_EXTS) || ctype.starts_with("image/") {
        DocFormat::Image
    } else if ends(&[".html", ".htm"]) || ctype.contains("html") {
        DocFormat::Html
    } else if ends(&[".md", ".markdown"]) {
        DocFormat::Markdown
    } else if code_lang(&ext(source)).is_some() || !structured_text_lang(&ctype).is_empty() {
        DocFormat::Code
    } else if ends(&[".txt", ".log"]) || ctype.starts_with("text/") {
        DocFormat::Text
    } else {
        DocFormat::Unknown
    }
}

/// Classify a body that name and media type left `Unknown` by its bytes:
/// `%PDF-`, an OOXML/EPUB/plain ZIP container, or plain text (no NUL bytes
/// and valid UTF-8 in the first 8 KiB). `None` keeps it unsupported.
pub(crate) fn sniff_unknown(blob: &[u8]) -> Option<DocFormat> {
    if looks_like_pdf(blob) {
        return Some(DocFormat::Pdf);
    }
    if blob.starts_with(b"PK\x03\x04") || blob.starts_with(b"PK\x05\x06") {
        return Some(sniff_zip_container(blob));
    }
    let head = &blob[..blob.len().min(8192)];
    if head.is_empty() || head.contains(&0) {
        return None;
    }
    match std::str::from_utf8(head) {
        Ok(_) => Some(DocFormat::Text),
        // A multi-byte character cut by the 8 KiB window is still text.
        Err(e) if e.error_len().is_none() && head.len() == 8192 => Some(DocFormat::Text),
        Err(_) => None,
    }
}

/// `%PDF-` within the first KiB (readers tolerate leading junk there).
pub(crate) fn looks_like_pdf(blob: &[u8]) -> bool {
    let head = &blob[..blob.len().min(1024)];
    head.windows(5).any(|w| w == b"%PDF-")
}

fn sniff_zip_container(blob: &[u8]) -> DocFormat {
    let Ok(archive) = zip::ZipArchive::new(std::io::Cursor::new(blob)) else {
        return DocFormat::Archive;
    };
    if archive.len() > crate::office::MAX_ARCHIVE_ENTRIES {
        return DocFormat::Archive;
    }
    let (mut has_container, mut has_mimetype) = (false, false);
    for name in archive.file_names().flatten() {
        if name.eq_ignore_ascii_case("word/document.xml") {
            return DocFormat::Docx;
        } else if name.eq_ignore_ascii_case("xl/workbook.xml") {
            return DocFormat::Xlsx;
        } else if name.eq_ignore_ascii_case("ppt/presentation.xml") {
            return DocFormat::Pptx;
        } else if name.eq_ignore_ascii_case("META-INF/container.xml") {
            has_container = true;
        } else if name.eq_ignore_ascii_case("mimetype") {
            has_mimetype = true;
        }
    }
    if has_container && has_mimetype {
        DocFormat::Epub
    } else {
        DocFormat::Archive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_format_by_extension() {
        let cases = [
            ("a.pdf", "pdf"),
            ("a.docx", "docx"),
            ("a.xlsx", "xlsx"),
            ("a.pptx", "pptx"),
            ("a.epub", "epub"),
            ("a.csv", "csv"),
            ("a.tsv", "csv"),
            ("a.zip", "archive"),
            ("a.tar.gz", "archive"),
            ("a.whl", "archive"),
            ("a.png", "image"),
            ("a.svg", "image"),
            ("a.heic", "image"),
            ("a.py", "code"),
            ("a.rs", "code"),
            ("a.json", "code"),
            ("a.yaml", "code"),
            ("Dockerfile", "code"),
            ("Makefile", "code"),
            ("a.html", "html"),
            ("a.md", "markdown"),
            ("a.txt", "text"),
            ("a.bin", "unknown"),
        ];
        for (source, expected) in cases {
            assert_eq!(detect_format(source, None).as_str(), expected, "{source}");
        }
    }

    #[test]
    fn test_detect_format_by_content_type() {
        let cases = [
            ("application/pdf", "pdf"),
            (
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "xlsx",
            ),
            (
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                "pptx",
            ),
            ("application/epub+zip", "epub"),
            ("text/csv", "csv"),
            ("application/zip", "archive"),
            ("image/png", "image"),
            ("image/svg+xml", "image"),
            ("text/plain; charset=utf-8", "text"),
            ("application/json", "code"),
            ("application/json; charset=utf-8", "code"),
            ("application/ld+json", "code"),
            ("application/atom+xml", "code"),
            ("application/yaml", "code"),
            ("application/xhtml+xml", "html"),
            ("application/octet-stream", "unknown"),
        ];
        for (ctype, expected) in cases {
            assert_eq!(
                detect_format("https://x.example/download?id=1", Some(ctype)).as_str(),
                expected,
                "{ctype}"
            );
        }
    }

    #[test]
    fn test_query_string_does_not_confuse_extension_detection() {
        assert_eq!(
            detect_format("https://x.example/data.csv?token=abc.png", None),
            DocFormat::Csv
        );
        assert_eq!(ext("https://x.example/a/B.PY?x=1.rs#frag.go"), "py");
        assert_eq!(ext("noext"), "");
        assert_eq!(ext("dir/CMakeLists.txt"), "cmakelists");
    }

    #[test]
    fn structured_text_lang_covers_suffixes() {
        assert_eq!(
            structured_text_lang("application/problem+json; charset=x"),
            "json"
        );
        assert_eq!(structured_text_lang("application/x-ndjson"), "json");
        assert_eq!(structured_text_lang("text/xml"), "xml");
        assert_eq!(structured_text_lang("application/x-yaml"), "yaml");
        assert_eq!(structured_text_lang("text/plain"), "");
    }

    #[test]
    fn sniffing_rescues_octet_stream_bodies_only_when_sure() {
        assert_eq!(sniff_unknown(b"%PDF-1.7\n..."), Some(DocFormat::Pdf));
        assert_eq!(
            sniff_unknown(b"\xef\xbb\xbfjunk%PDF-1.4"),
            Some(DocFormat::Pdf)
        );
        assert_eq!(sniff_unknown(b"plain words\n"), Some(DocFormat::Text));
        assert_eq!(sniff_unknown(b"\x00\x01\x02"), None);
        assert_eq!(sniff_unknown(b""), None);
        assert_eq!(sniff_unknown(b"\xff\xfe\xfa binary"), None);
        assert_eq!(
            sniff_unknown(b"PK\x03\x04 truncated"),
            Some(DocFormat::Archive)
        );
    }
}
