//! Port of `tests/test_fetch_charset_doc.py` + `chardetng` fallback tests.

use encoding_rs::{GBK, SHIFT_JIS};
use fsm_extract::charset::decode_body;
use fsm_extract::fetch::{ctype_is_markup, is_document_ctype, is_document_url};

#[test]
fn test_decode_honors_header_charset_gbk() {
    let text = "中文内容";
    let (body, _, _) = GBK.encode(text);
    let out = decode_body(&body, "text/html; charset=gbk", None);
    assert_eq!(out, text);
}

#[test]
fn test_decode_sniffs_meta_charset_when_header_lacks_one() {
    let text = "日本語のテスト";
    let (sjis, _, _) = SHIFT_JIS.encode(text);
    let mut body = Vec::new();
    body.extend_from_slice(b"<html><head><meta charset='shift_jis'></head><body>");
    body.extend_from_slice(&sjis);
    body.extend_from_slice(b"</body></html>");
    let out = decode_body(&body, "text/html", None);
    assert!(out.contains(text), "got: {out}");
}

#[test]
fn test_decode_defaults_to_utf8_and_never_raises_on_bad_codec() {
    let text = "ünïcode ✓ 中文";
    let body = text.as_bytes();
    assert_eq!(decode_body(body, "text/html", None), text);
    assert_eq!(
        decode_body(body, "text/html; charset=not-a-real-codec", None),
        text
    );
}

#[test]
fn test_decode_chardetng_detects_undeclared_gbk_and_shift_jis() {
    let zh =
        "这是一个没有声明字符集头的简体中文测试页面，用于验证自动编码检测是否正确识别中文字符。";
    let (gbk_bytes, _, _) = GBK.encode(zh);
    let out_zh = decode_body(&gbk_bytes, "text/html", Some("https://example.cn/page"));
    assert_eq!(out_zh, zh);

    let ja = "これは文字コード宣言がない日本語ページのテストです。自動的にエンコーディングを判別します。";
    let (sjis_bytes, _, _) = SHIFT_JIS.encode(ja);
    let out_ja = decode_body(&sjis_bytes, "text/html", Some("https://example.jp/page"));
    assert_eq!(out_ja, ja);
}

#[test]
fn test_decode_honors_utf8_bom() {
    let mut body = vec![0xEF, 0xBB, 0xBF];
    body.extend_from_slice("hello 世界".as_bytes());
    assert_eq!(decode_body(&body, "text/plain", None), "hello 世界");
}

#[test]
fn test_is_document_url_matches_pdf_and_docx_suffixes() {
    assert!(is_document_url("https://arxiv.org/pdf/1706.03762.pdf"));
    assert!(is_document_url("https://example.com/report.docx"));
    assert!(is_document_url("https://example.com/paper.PDF?download=1"));
    assert!(is_document_url("https://example.com/sheet.xlsx"));
    assert!(is_document_url("https://example.com/slides.pptx"));
    assert!(is_document_url("https://example.com/book.epub"));
    assert!(!is_document_url("https://example.com/article.html"));
    assert!(!is_document_url("https://example.com/"));
}

#[test]
fn test_is_document_ctype_matches_binary_doc_types() {
    assert!(is_document_ctype("application/pdf"));
    assert!(is_document_ctype(
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    ));
    assert!(is_document_ctype(
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    ));
    assert!(!is_document_ctype("text/html; charset=utf-8"));
    assert!(!is_document_ctype("application/json"));
}

#[test]
fn test_ctype_is_markup_treats_empty_as_recoverable() {
    assert!(ctype_is_markup(""));
    assert!(ctype_is_markup("text/html"));
    assert!(ctype_is_markup("application/xml"));
    assert!(!ctype_is_markup("application/json"));
    assert!(!ctype_is_markup("text/plain"));
}
