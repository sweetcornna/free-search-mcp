//! Body decoding (port of `httpfetch._decode_body`): Content-Type charset >
//! HTML `<meta>` / XML declaration charset > UTF-8. Unknown labels fall back
//! to UTF-8 with replacement, so decoding never fails.
//!
//! Extension over the oracle: when nothing declares a charset and the bytes
//! are not valid UTF-8 (a GBK `.txt` under DOCUMENT_ROOT), `chardetng`
//! guesses instead of producing a page of U+FFFD.

use encoding_rs::{Encoding, UTF_8};
use regex::Regex;
use regex::bytes::Regex as BytesRegex;
use std::borrow::Cow;
use std::sync::LazyLock;

static CTYPE_CHARSET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)charset\s*=\s*["']?([\w\-]+)"#).expect("regex"));
static META_CHARSET: LazyLock<BytesRegex> = LazyLock::new(|| {
    BytesRegex::new(r#"(?i)<meta[^>]+charset\s*=\s*["']?\s*([a-zA-Z0-9_\-]+)"#).expect("regex")
});
static XML_ENCODING: LazyLock<BytesRegex> = LazyLock::new(|| {
    BytesRegex::new(
        r#"^(?-u:\xef\xbb\xbf)?\s*<\?xml[^>]*?encoding\s*=\s*["']([A-Za-z0-9_.:\-]+)["']"#,
    )
    .expect("regex")
});

/// `_charset_from_ctype`.
pub(crate) fn charset_from_ctype(ctype: &str) -> Option<String> {
    CTYPE_CHARSET
        .captures(ctype)
        .map(|c| c[1].to_ascii_lowercase())
}

/// `_sniff_meta_charset` over the first 4 KiB, then an XML declaration
/// (XHTML inside EPUB/OOXML carries its encoding there).
pub(crate) fn sniff_declared_charset(body: &[u8]) -> Option<String> {
    let head = &body[..body.len().min(4096)];
    META_CHARSET
        .captures(head)
        .or_else(|| XML_ENCODING.captures(head))
        .and_then(|c| std::str::from_utf8(&c[1]).ok().map(str::to_ascii_lowercase))
}

/// `_decode_body(body, ctype)`.
pub(crate) fn decode_body(body: &[u8], ctype: &str) -> String {
    let ctype_l = ctype.to_ascii_lowercase();
    let mut label = charset_from_ctype(ctype);
    if label.is_none() && (ctype.is_empty() || ctype_l.contains("html") || ctype_l.contains("xml"))
    {
        label = sniff_declared_charset(body);
    }
    match label {
        Some(l) => match Encoding::for_label(l.as_bytes()) {
            // A replacement-only label (e.g. "iso-2022-kr") would turn the
            // whole body into U+FFFD; Python falls back to UTF-8 there too.
            Some(enc) if enc != encoding_rs::REPLACEMENT => enc.decode(body).0.into_owned(),
            _ => UTF_8.decode(body).0.into_owned(),
        },
        None => decode_undeclared(body),
    }
}

/// No declared charset: UTF-8 if it is UTF-8 (BOM honoured), else guess.
fn decode_undeclared(body: &[u8]) -> String {
    if let Some((enc, _)) = Encoding::for_bom(body) {
        return enc.decode(body).0.into_owned();
    }
    if let Ok(s) = std::str::from_utf8(body) {
        return s.to_owned();
    }
    let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    det.feed(&body[..body.len().min(64 * 1024)], body.len() <= 64 * 1024);
    let enc = det.guess(None, chardetng::Utf8Detection::Allow);
    let (text, _, _): (Cow<'_, str>, _, _) = enc.decode(body);
    text.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_honors_header_charset_gbk() {
        let (body, _, _) = encoding_rs::GBK.encode("中文内容");
        assert_eq!(decode_body(&body, "text/html; charset=gbk"), "中文内容");
    }

    #[test]
    fn test_decode_sniffs_meta_charset_when_header_lacks_one() {
        let (sjis, _, _) = encoding_rs::SHIFT_JIS.encode("日本語のテスト");
        let mut body = b"<html><head><meta charset='shift_jis'></head><body>".to_vec();
        body.extend_from_slice(&sjis);
        body.extend_from_slice(b"</body></html>");
        assert!(decode_body(&body, "text/html").contains("日本語のテスト"));
    }

    #[test]
    fn test_decode_defaults_to_utf8_and_never_raises_on_bad_codec() {
        let text = "ünïcode ✓ 中文";
        assert_eq!(decode_body(text.as_bytes(), "text/html"), text);
        assert_eq!(
            decode_body(text.as_bytes(), "text/html; charset=not-a-real-codec"),
            text
        );
    }

    #[test]
    fn test_read_doc_text_parser_honours_header_charset() {
        let (gbk, _, _) = encoding_rs::GBK.encode("中文标题");
        assert_eq!(decode_body(&gbk, "text/plain; charset=GBK"), "中文标题");
    }

    #[test]
    fn test_read_doc_html_parser_sniffs_meta_charset_without_a_header() {
        let (gbk, _, _) = encoding_rs::GBK
            .encode("<html><head><meta charset='gbk'></head><body><p>中文</p></body></html>");
        assert!(decode_body(&gbk, "").contains("中文"));
    }

    #[test]
    fn test_unsupported_codecs_use_utf8_replacement() {
        for enc in ["idna", "undefined", "base64_codec", "not_a_codec"] {
            assert_eq!(
                decode_body(b"hello\xff", &format!("text/plain; charset={enc}")),
                "hello\u{fffd}",
                "{enc}"
            );
        }
    }

    #[test]
    fn xml_declaration_and_meta_are_sniffed_only_for_markup_or_unknown_types() {
        let (latin, _, _) = encoding_rs::WINDOWS_1252
            .encode("<?xml version=\"1.0\" encoding=\"windows-1252\"?><p>caf\u{e9}</p>");
        assert!(decode_body(&latin, "application/xhtml+xml").contains("café"));
        assert!(decode_body(&latin, "").contains("café"));
        assert_eq!(
            charset_from_ctype("text/html; Charset=\"UTF-8\""),
            Some("utf-8".into())
        );
    }

    #[test]
    fn undeclared_non_utf8_text_is_guessed_not_replaced() {
        let (gbk, _, _) = encoding_rs::GBK
            .encode("这是一个没有声明字符集的中文文本文件，内容足够长以便检测器做出判断。");
        let out = decode_body(&gbk, "");
        assert!(out.contains("中文文本"), "{out}");
        assert!(!out.contains('\u{fffd}'));
        assert_eq!(decode_body(b"\xef\xbb\xbfplain", ""), "plain");
    }
}
