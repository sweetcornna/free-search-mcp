//! Parsers for the single-record APIs `fsm-scholar` calls directly
//! (OpenAlex, Semantic Scholar, Crossref, arXiv Atom, PMC idconv), each
//! mapping one upstream record to a [`ScholarlyWork`] tagged with its source.
//!
//! Public so `fsm-scholar-graph` / `fsm-scholar-fulltext` can reuse them.
//! Every parser is total: malformed or missing fields are skipped, never
//! a panic, and a record without a title is `None`.

use crate::ids::{normalize_arxiv, normalize_doi, normalize_work_ids};
use fsm_core::{AuthorRef, OaLocation, ScholarlyWork, WorkIds, WorkVersion};
use regex::Regex;
use roxmltree::ParsingOptions;
use serde_json::Value;
use std::sync::LazyLock;

/// Maximum XML bytes accepted by [`parse_xml_safe`] (R3 untrusted XML cap).
const MAX_XML_BYTES: usize = 4 * 1024 * 1024;
/// Maximum nodes parsed from an untrusted XML document.
const MAX_XML_NODES: u32 = 50_000;
/// Maximum bytes extracted from a single XML text field (R3 cap).
const MAX_XML_FIELD_BYTES: usize = 64 * 1024;

/// Matches HTML/XML comments and real markup tags (`<i>`, `</jats:p>`,
/// `<jats:sec id="s1">`, `<br />`), never math inequalities such as
/// `BMI < 25 ... > 10%` or `x<y and z>w`.
static TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)<!--.*?-->|</?[A-Za-z][A-Za-z0-9:_-]*(?:\s+[A-Za-z_:][A-Za-z0-9:._-]*\s*=\s*(?:"[^"]*"|'[^']*'|[^\s"'=<>`]+))*\s*/?>"#,
    )
    .unwrap_or_else(|e| panic!("tag regex: {e}"))
});

/// Matches a JATS/XML `<jats:title>Abstract</jats:title>` (or `<title>Abstract</title>`)
/// heading element so it is dropped before generic tag stripping.
static JATS_ABSTRACT_TITLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)^\s*<(?:[A-Za-z][A-Za-z0-9_-]*:)?title(?:\s[^<>]*)?>\s*abstract\s*</(?:[A-Za-z][A-Za-z0-9_-]*:)?title>")
        .unwrap_or_else(|e| panic!("jats abstract title regex: {e}"))
});

/// Parse untrusted upstream XML safely (R3): rejects DTD / `<!ENTITY`
/// declarations and enforces size and node caps.
pub(crate) fn parse_xml_safe(xml: &str) -> Option<roxmltree::Document<'_>> {
    if xml.len() > MAX_XML_BYTES {
        return None;
    }
    if xml.contains("<!ENTITY") || xml.contains("<!entity") {
        return None;
    }
    roxmltree::Document::parse_with_options(
        xml,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: MAX_XML_NODES,
            ..ParsingOptions::default()
        },
    )
    .ok()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cap_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Strip a leading standalone `"Abstract"` / `"ABSTRACT"` heading without
/// touching words that merely start with it (`"Abstraction"`, `"Abstracting"`,
/// `"Abstracts"`) or prose phrases (`"Abstract algebra ..."`).
fn strip_abstract_heading(t: &str) -> &str {
    let Some(rest) = t
        .strip_prefix("Abstract")
        .or_else(|| t.strip_prefix("ABSTRACT"))
    else {
        return t;
    };
    if rest.is_empty() {
        return "";
    }
    let after_ws = rest.trim_start();
    // Explicit heading delimiter (`Abstract: ...`, `Abstract. ...`, `Abstract — ...`, `Abstract - ...`).
    if let Some(body) = after_ws
        .strip_prefix(':')
        .or_else(|| after_ws.strip_prefix('.'))
        .or_else(|| after_ws.strip_prefix('—'))
        .or_else(|| after_ws.strip_prefix('–'))
        .or_else(|| {
            after_ws
                .strip_prefix('-')
                .filter(|r| r.starts_with(char::is_whitespace))
        })
    {
        return body.trim();
    }
    // Heading separated by whitespace from a capitalized sentence start (`Abstract This paper...`),
    // never mid-word (`Abstraction`) and never lowercase phrase (`Abstract algebra...`).
    if rest.starts_with(char::is_whitespace)
        && after_ws.chars().next().is_some_and(char::is_uppercase)
    {
        return after_ws;
    }
    t
}

/// Decode the handful of HTML/XML entities upstream text carries.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find(';').filter(|e| *e <= 10);
        let decoded = end.and_then(|e| {
            let name = &tail[1..e];
            let ch = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => name
                    .strip_prefix("#x")
                    .or_else(|| name.strip_prefix("#X"))
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| name.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            };
            ch.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &tail[e + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Markup removed (JATS `<jats:p>`, `<i>`, ...), entities decoded,
/// whitespace collapsed.
pub fn clean_text(s: &str) -> String {
    let no_tags = TAG_RE.replace_all(s, " ");
    collapse_ws(&decode_entities(&no_tags))
}

/// [`clean_text`] for abstracts, also dropping a leading "Abstract" heading.
pub fn clean_abstract(s: &str) -> Option<String> {
    let without_jats_title = JATS_ABSTRACT_TITLE_RE.replace(s, " ");
    let t = clean_text(&without_jats_title);
    let t = strip_abstract_heading(&t).trim();
    (!t.is_empty()).then(|| t.to_owned())
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn u64_of(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

fn year_of(date: &str) -> Option<u16> {
    let y: u16 = date.get(..4)?.parse().ok()?;
    (1000..=2999).contains(&y).then_some(y)
}

/// `{"date-parts": [[2020, 5, 22]]}` → `2020-05-22`, truncating at the first
/// gap or out-of-range part (port of `paper_graph._crossref_date` with the
/// range checks of `CrossrefEngine._issued_date`). `None` when there is no year.
pub fn crossref_date(v: &Value) -> Option<String> {
    let parts = v.get("date-parts")?.as_array()?.first()?.as_array()?;
    let nums: Vec<i64> = parts.iter().map_while(Value::as_i64).take(3).collect();
    let year = *nums.first()?;
    if !(1..=9999).contains(&year) {
        return None;
    }
    let mut out = format!("{year:04}");
    if let Some(m) = nums.get(1).filter(|m| (1..=12).contains(*m)) {
        out.push_str(&format!("-{m:02}"));
        if let Some(d) = nums.get(2).filter(|d| (1..=31).contains(*d)) {
            out.push_str(&format!("-{d:02}"));
        }
    }
    Some(out)
}

/// Rebuild prose from an OpenAlex `{word: [positions]}` inverted index.
pub fn openalex_abstract(inverted: &Value) -> Option<String> {
    let map = inverted.as_object()?;
    let mut positions: Vec<(u64, &str)> = Vec::new();
    for (word, idxs) in map {
        for i in idxs
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
        {
            positions.push((i, word.as_str()));
        }
    }
    if positions.is_empty() {
        return None;
    }
    positions.sort_by_key(|(i, _)| *i);
    positions.dedup_by_key(|(i, _)| *i);
    let text: Vec<&str> = positions.into_iter().map(|(_, w)| w).collect();
    clean_abstract(&text.join(" "))
}

fn doi_url(doi: &str) -> String {
    format!("https://doi.org/{doi}")
}

/// Canonical https form of resolver URLs (`http://dx.doi.org/...`).
pub fn canonical_url(url: &str) -> String {
    let u = url.trim();
    for prefix in [
        "http://dx.doi.org/",
        "https://dx.doi.org/",
        "http://doi.org/",
        "http://www.doi.org/",
    ] {
        if let Some(rest) = u.strip_prefix(prefix) {
            return format!("https://doi.org/{rest}");
        }
    }
    if let Some(rest) = u.strip_prefix("http://arxiv.org/") {
        return format!("https://arxiv.org/{rest}");
    }
    u.to_owned()
}

fn finish(mut w: ScholarlyWork) -> Option<ScholarlyWork> {
    w.title = w.title.trim().to_owned();
    if w.title.is_empty() {
        return None;
    }
    normalize_work_ids(&mut w.ids);
    if w.url.is_empty() {
        if let Some(d) = &w.ids.doi {
            w.url = doi_url(d);
        } else if let Some(a) = &w.ids.arxiv {
            w.url = format!("https://arxiv.org/abs/{a}");
        }
    }
    w.url = canonical_url(&w.url);
    Some(w)
}

/// One OpenAlex work record (`/works/{id}` or an item of `results`).
pub fn openalex_work(v: &Value) -> Option<ScholarlyWork> {
    let title = clean_text(str_of(v, "display_name").or_else(|| str_of(v, "title"))?);
    let mut w = ScholarlyWork::new(title, "");
    w.sources = vec!["openalex".into()];
    w.work_type = str_of(v, "type").unwrap_or("article").to_owned();
    w.published_date = str_of(v, "publication_date").map(str::to_owned);
    w.year = v
        .get("publication_year")
        .and_then(Value::as_u64)
        .and_then(|y| u16::try_from(y).ok())
        .or_else(|| w.published_date.as_deref().and_then(year_of));
    let primary = v.get("primary_location").filter(|p| p.is_object());
    w.venue = primary
        .and_then(|p| p.get("source"))
        .and_then(|s| str_of(s, "display_name"))
        .map(clean_text)
        .filter(|s| !s.is_empty());
    for a in v
        .get("authorships")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let author = a.get("author").unwrap_or(&Value::Null);
        let Some(name) = str_of(author, "display_name")
            .or_else(|| str_of(a, "raw_author_name"))
            .map(clean_text)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let mut r = AuthorRef::new(name);
        r.orcid = str_of(author, "orcid").map(|o| o.rsplit('/').next().unwrap_or(o).to_owned());
        r.affiliation = a
            .get("institutions")
            .and_then(Value::as_array)
            .and_then(|i| i.first())
            .and_then(|i| str_of(i, "display_name"))
            .or_else(|| {
                a.get("raw_affiliation_strings")
                    .and_then(Value::as_array)
                    .and_then(|r| r.first())
                    .and_then(Value::as_str)
            })
            .map(str::to_owned);
        w.authors.push(r);
    }
    let ids = v.get("ids").unwrap_or(&Value::Null);
    w.ids.openalex = str_of(v, "id").map(str::to_owned);
    w.ids.doi = str_of(v, "doi")
        .or_else(|| str_of(ids, "doi"))
        .map(str::to_owned);
    w.ids.pmid = str_of(ids, "pmid").map(str::to_owned);
    w.ids.pmcid = str_of(ids, "pmcid").map(str::to_owned);
    w.ids.arxiv = str_of(ids, "arxiv").and_then(normalize_arxiv);
    // An arXiv landing page anywhere in the record links the preprint.
    for loc in [primary, v.get("best_oa_location")].into_iter().flatten() {
        if w.ids.arxiv.is_none()
            && let Some(u) = str_of(loc, "landing_page_url").filter(|u| u.contains("arxiv.org/"))
        {
            w.ids.arxiv = normalize_arxiv(u);
        }
    }
    w.abstract_text = v.get("abstract_inverted_index").and_then(openalex_abstract);
    w.cited_by = u64_of(v, "cited_by_count");
    if w.cited_by.is_some() {
        w.cited_by_source = Some("openalex".into());
    }
    w.retracted = v
        .get("is_retracted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let oa = v.get("open_access").unwrap_or(&Value::Null);
    let best = v.get("best_oa_location").filter(|b| b.is_object());
    match oa.get("is_oa").and_then(Value::as_bool) {
        Some(true) => {
            let mut loc = OaLocation::new(str_of(oa, "oa_status").unwrap_or("green"));
            loc.url = best
                .and_then(|b| str_of(b, "landing_page_url"))
                .or_else(|| str_of(oa, "oa_url"))
                .map(str::to_owned);
            loc.pdf_url = best.and_then(|b| str_of(b, "pdf_url")).map(str::to_owned);
            loc.license = best.and_then(|b| str_of(b, "license")).map(str::to_owned);
            w.oa = Some(loc);
        }
        Some(false) => w.oa = Some(OaLocation::new("closed")),
        None => {}
    }
    w.url = primary
        .and_then(|p| str_of(p, "landing_page_url"))
        .filter(|u| u.starts_with("http"))
        .map(str::to_owned)
        .unwrap_or_default();
    if w.url.is_empty() && w.ids.doi.is_none() {
        w.url = str_of(v, "id").unwrap_or_default().to_owned();
    }
    finish(w)
}

/// One Semantic Scholar Graph API paper (`/graph/v1/paper/{id}`).
pub fn s2_paper(v: &Value) -> Option<ScholarlyWork> {
    let title = clean_text(str_of(v, "title")?);
    let mut w = ScholarlyWork::new(title, "");
    w.sources = vec!["semanticscholar".into()];
    let ext = v.get("externalIds").unwrap_or(&Value::Null);
    let id_str = |k: &str| match ext.get(k) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    w.ids = WorkIds::default();
    w.ids.s2 = str_of(v, "paperId").map(str::to_owned);
    w.ids.doi = id_str("DOI");
    w.ids.arxiv = id_str("ArXiv");
    w.ids.pmid = id_str("PubMed");
    w.ids.pmcid = id_str("PubMedCentral");
    w.ids.dblp = id_str("DBLP");
    w.published_date = str_of(v, "publicationDate").map(str::to_owned);
    w.year = v
        .get("year")
        .and_then(Value::as_u64)
        .and_then(|y| u16::try_from(y).ok())
        .or_else(|| w.published_date.as_deref().and_then(year_of));
    w.venue = str_of(v, "venue").map(clean_text).filter(|s| !s.is_empty());
    let has_published_doi = w
        .ids
        .doi
        .as_deref()
        .and_then(normalize_doi)
        .is_some_and(|d| !crate::ids::is_preprint_doi(&d));
    w.work_type = if !has_published_doi && w.ids.arxiv.is_some() {
        "preprint".into()
    } else {
        "article".into()
    };
    for a in v
        .get("authors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(name) = str_of(a, "name").map(clean_text).filter(|s| !s.is_empty()) {
            w.authors.push(AuthorRef::new(name));
        }
    }
    w.abstract_text = str_of(v, "abstract").and_then(clean_abstract);
    w.cited_by = u64_of(v, "citationCount");
    if w.cited_by.is_some() {
        w.cited_by_source = Some("semanticscholar".into());
    }
    w.influential_citations = u64_of(v, "influentialCitationCount");
    if let Some(pdf) = v.get("openAccessPdf").filter(|p| p.is_object())
        && let Some(url) = str_of(pdf, "url")
    {
        let status = str_of(pdf, "status")
            .map(str::to_ascii_lowercase)
            .unwrap_or_else(|| "green".into());
        let mut loc = OaLocation::new(status);
        loc.pdf_url = Some(url.to_owned());
        loc.license = str_of(pdf, "license").map(str::to_owned);
        w.oa = Some(loc);
    }
    w.url = w
        .ids
        .s2
        .as_ref()
        .map(|id| format!("https://www.semanticscholar.org/paper/{id}"))
        .unwrap_or_default();
    finish(w)
}

/// Crossref `type` → the work types the rest of the system uses.
fn crossref_type(t: &str) -> String {
    match t {
        "journal-article" => "article".into(),
        "posted-content" => "preprint".into(),
        other => other.to_owned(),
    }
}

fn first_str(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .find(|s| !s.trim().is_empty())
            .map(str::to_owned),
        _ => None,
    }
}

/// One Crossref work (the `message` of `/works/{doi}`, or an item of
/// `message.items`).
pub fn crossref_work(v: &Value) -> Option<ScholarlyWork> {
    let title = clean_text(&first_str(v.get("title"))?);
    let mut w = ScholarlyWork::new(title, "");
    w.sources = vec!["crossref".into()];
    w.ids.doi = str_of(v, "DOI").map(str::to_owned);
    w.work_type = crossref_type(str_of(v, "type").unwrap_or("journal-article"));
    w.published_date = ["issued", "published", "published-print", "published-online"]
        .iter()
        .find_map(|k| v.get(*k).and_then(crossref_date));
    w.year = w.published_date.as_deref().and_then(year_of);
    w.venue = first_str(v.get("container-title"))
        .or_else(|| {
            v.get("institution")
                .and_then(Value::as_array)
                .and_then(|i| i.first())
                .and_then(|i| str_of(i, "name"))
                .map(str::to_owned)
        })
        .map(|s| clean_text(&s))
        .filter(|s| !s.is_empty());
    for a in v
        .get("author")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let raw_name = match (str_of(a, "given"), str_of(a, "family"), str_of(a, "name")) {
            (Some(g), Some(f), _) => format!("{g} {f}"),
            (None, Some(f), _) => f.to_owned(),
            (_, None, Some(n)) => n.to_owned(),
            _ => continue,
        };
        let name = clean_text(&raw_name);
        if name.is_empty() {
            continue;
        }
        let mut r = AuthorRef::new(name);
        r.orcid = str_of(a, "ORCID").map(|o| o.rsplit('/').next().unwrap_or(o).to_owned());
        r.affiliation = a
            .get("affiliation")
            .and_then(Value::as_array)
            .and_then(|x| x.first())
            .and_then(|x| str_of(x, "name"))
            .map(str::to_owned);
        w.authors.push(r);
    }
    w.abstract_text = str_of(v, "abstract").and_then(clean_abstract);
    w.cited_by = u64_of(v, "is-referenced-by-count");
    if w.cited_by.is_some() {
        w.cited_by_source = Some("crossref".into());
    }
    for u in v
        .get("updated-by")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let kind = str_of(u, "type").unwrap_or("").to_ascii_lowercase();
        if kind == "retraction" || kind == "withdrawal" || kind == "removal" {
            w.retracted = true;
            let date = u.get("updated").and_then(crossref_date);
            let notice = str_of(u, "DOI").unwrap_or("");
            w.retraction_reason = Some(match date {
                Some(d) => format!("Crossref {kind} notice {notice} ({d})"),
                None => format!("Crossref {kind} notice {notice}"),
            });
            break;
        }
    }
    let relation = v.get("relation").unwrap_or(&Value::Null);
    for (rel, kind) in [
        ("is-preprint-of", "published"),
        ("has-preprint", "preprint"),
    ] {
        for r in relation
            .get(rel)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if str_of(r, "id-type").is_some_and(|t| t.eq_ignore_ascii_case("doi"))
                && let Some(doi) = str_of(r, "id").and_then(normalize_doi)
            {
                let mut ver = WorkVersion::new(kind, doi_url(&doi));
                if let Some(a) = crate::ids::arxiv_of_doi(&doi) {
                    ver.arxiv = Some(a);
                } else {
                    ver.doi = Some(doi);
                }
                w.versions.push(ver);
            }
        }
    }
    w.url = str_of(v, "URL").map(str::to_owned).unwrap_or_default();
    finish(w)
}

/// Every item of a Crossref `/works?query...` page.
pub fn crossref_items(v: &Value) -> Vec<ScholarlyWork> {
    v.get("message")
        .and_then(|m| m.get("items"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(crossref_work)
        .collect()
}

const ATOM: &str = "http://www.w3.org/2005/Atom";
const ARXIV_NS: &str = "http://arxiv.org/schemas/atom";

/// Every `<entry>` of an arXiv Atom feed (`export.arxiv.org/api/query`).
/// Error entries (`id` outside `arxiv.org/abs/`), DTD / entity declarations
/// (R3), and malformed XML yield nothing.
pub fn arxiv_feed(xml: &str) -> Vec<ScholarlyWork> {
    let Some(doc) = parse_xml_safe(xml) else {
        return Vec::new();
    };
    // roxmltree has already decoded XML entities (`&lt;`, `&gt;`, `&amp;`), so
    // arXiv Atom text is plain text/LaTeX: collapse whitespace only, never strip
    // `<...>` or re-decode entities.
    let child_text = |n: roxmltree::Node<'_, '_>, ns: &str, name: &str| -> Option<String> {
        n.children()
            .find(|c| c.has_tag_name((ns, name)))
            .and_then(|c| c.text())
            .map(|t| collapse_ws(cap_bytes(t, MAX_XML_FIELD_BYTES)))
            .filter(|s| !s.is_empty())
    };
    let mut out = Vec::new();
    for entry in doc
        .root_element()
        .children()
        .filter(|c| c.has_tag_name((ATOM, "entry")))
    {
        let Some(id_url) = child_text(entry, ATOM, "id") else {
            continue;
        };
        if !id_url.contains("arxiv.org/abs/") {
            continue;
        }
        let Some(arxiv) = normalize_arxiv(&id_url) else {
            continue;
        };
        let Some(title) = child_text(entry, ATOM, "title") else {
            continue;
        };
        let mut w = ScholarlyWork::new(title, format!("https://arxiv.org/abs/{arxiv}"));
        w.sources = vec!["arxiv".into()];
        w.work_type = "preprint".into();
        w.ids.arxiv = Some(arxiv.clone());
        w.ids.doi = child_text(entry, ARXIV_NS, "doi").and_then(|d| normalize_doi(&d));
        w.published_date =
            child_text(entry, ATOM, "published").map(|p| p.chars().take(10).collect());
        w.year = w.published_date.as_deref().and_then(year_of);
        w.venue =
            Some(child_text(entry, ARXIV_NS, "journal_ref").unwrap_or_else(|| "arXiv".into()));
        w.abstract_text = child_text(entry, ATOM, "summary").and_then(|s| {
            let t = strip_abstract_heading(&s).trim();
            (!t.is_empty()).then(|| t.to_owned())
        });
        for a in entry
            .children()
            .filter(|c| c.has_tag_name((ATOM, "author")))
        {
            if let Some(name) = child_text(a, ATOM, "name") {
                let mut r = AuthorRef::new(name);
                r.affiliation = child_text(a, ARXIV_NS, "affiliation");
                w.authors.push(r);
            }
        }
        let mut oa = OaLocation::new("green");
        oa.url = Some(w.url.clone());
        oa.pdf_url = entry
            .children()
            .filter(|c| c.has_tag_name((ATOM, "link")))
            .find(|l| {
                l.attribute("title") == Some("pdf")
                    || l.attribute("type") == Some("application/pdf")
            })
            .and_then(|l| l.attribute("href"))
            .map(canonical_url)
            .or_else(|| Some(format!("https://arxiv.org/pdf/{arxiv}")));
        oa.structured_format = Some("html5".into());
        w.oa = Some(oa);
        if let Some(w) = finish(w) {
            out.push(w);
        }
    }
    out
}

/// The ids PMC idconv reports for one requested id (`records[0]`).
pub fn idconv_ids(v: &Value) -> Option<WorkIds> {
    let rec = v.get("records")?.as_array()?.first()?;
    if str_of(rec, "status").is_some_and(|s| s.eq_ignore_ascii_case("error")) {
        return None;
    }
    let mut ids = WorkIds::default();
    ids.doi = str_of(rec, "doi").map(str::to_owned);
    ids.pmid = match rec.get("pmid") {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    ids.pmcid = str_of(rec, "pmcid").map(str::to_owned);
    normalize_work_ids(&mut ids);
    (!ids.is_empty()).then_some(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Port of `test_paper_graph.py::test_crossref_date_truncates_at_the_first_gap`.
    #[test]
    fn test_crossref_date_truncates_at_the_first_gap() {
        let cases = [
            (json!({"date-parts": [[2020, 6, 5]]}), Some("2020-06-05")),
            (json!({"date-parts": [[2020, 6]]}), Some("2020-06")),
            (json!({"date-parts": [[2020]]}), Some("2020")),
            // Truncate at the gap rather than promoting the day into the month slot.
            (json!({"date-parts": [[2020, null, 5]]}), Some("2020")),
            (json!({"date-parts": [[null]]}), None),
            (json!({}), None),
            (Value::Null, None),
            (json!({"date-parts": [[2024, 13, 5]]}), Some("2024")),
        ];
        for (v, want) in cases {
            assert_eq!(crossref_date(&v).as_deref(), want, "{v}");
        }
    }

    /// Port of `test_academic_engines.py::test_openalex_rebuilds_prose_from_the_inverted_abstract`.
    #[test]
    fn test_openalex_rebuilds_prose_from_the_inverted_abstract() {
        let inv = json!({"The": [0], "dominant": [1], "models": [2], "are": [3], "complex": [4]});
        assert_eq!(
            openalex_abstract(&inv).as_deref(),
            Some("The dominant models are complex")
        );
        assert_eq!(openalex_abstract(&json!("nope")), None);
        assert_eq!(openalex_abstract(&json!({})), None);
    }

    /// Port of `test_academic_engines.py::test_crossref_strips_jats_markup_from_abstracts`
    /// and `test_crossref_unwraps_list_titles`.
    #[test]
    fn test_crossref_strips_jats_markup_from_abstracts() {
        let w = crossref_work(&json!({
            "title": ["The Triple Attention Transformer"],
            "URL": "http://dx.doi.org/10.1234/x",
            "DOI": "10.1234/X",
            "issued": {"date-parts": [[2024, 2, 5]]},
            "author": [{"given": "Shadi", "family": "Ghaith", "ORCID": "http://orcid.org/0000-0001-2345-6789"}],
            "container-title": ["Nature"],
            "abstract": "<jats:title>Abstract</jats:title><jats:p>This paper introduces the model &amp; more.</jats:p>",
            "is-referenced-by-count": 7,
            "type": "journal-article"
        }))
        .unwrap();
        assert_eq!(w.title, "The Triple Attention Transformer");
        assert_eq!(
            w.abstract_text.as_deref(),
            Some("This paper introduces the model & more.")
        );
        assert_eq!(w.ids.doi.as_deref(), Some("10.1234/x"));
        assert_eq!(w.url, "https://doi.org/10.1234/x");
        assert_eq!(
            (w.year, w.published_date.as_deref()),
            (Some(2024), Some("2024-02-05"))
        );
        assert_eq!(w.authors[0].name, "Shadi Ghaith");
        assert_eq!(w.authors[0].orcid.as_deref(), Some("0000-0001-2345-6789"));
        assert_eq!(
            (w.cited_by, w.cited_by_source.as_deref()),
            (Some(7), Some("crossref"))
        );
        assert_eq!(w.work_type, "article");
        // Port of test_crossref_skips_entries_without_a_title.
        assert!(crossref_work(&json!({"title": [], "URL": "https://doi.org/10.1/w"})).is_none());
    }

    #[test]
    fn crossref_retraction_and_preprint_relations() {
        let w = crossref_work(&json!({
            "title": ["Hydroxychloroquine or chloroquine"],
            "DOI": "10.1016/S0140-6736(20)31180-6",
            "updated-by": [
                {"type": "expression_of_concern", "DOI": "10.1016/s0140-6736(20)31290-3", "updated": {"date-parts": [[2020, 6, 3]]}},
                {"type": "retraction", "DOI": "10.1016/s0140-6736(20)31324-6", "updated": {"date-parts": [[2020, 6, 5]]}}
            ],
            "relation": {"has-preprint": [{"id-type": "doi", "id": "10.1101/2020.01.01.000001"}]}
        }))
        .unwrap();
        assert!(w.retracted);
        assert!(
            w.retraction_reason
                .as_deref()
                .unwrap()
                .contains("2020-06-05")
        );
        assert_eq!(w.versions.len(), 1);
        assert_eq!(w.versions[0].kind, "preprint");
        assert_eq!(w.ids.doi.as_deref(), Some("10.1016/s0140-6736(20)31180-6"));
    }

    #[test]
    fn openalex_record_maps_every_field() {
        let w = openalex_work(&json!({
            "id": "https://openalex.org/W2148972377",
            "doi": "https://doi.org/10.1145/1571941.1572114",
            "display_name": "Reciprocal rank fusion outperforms <i>condorcet</i>",
            "publication_year": 2009,
            "publication_date": "2009-07-19",
            "type": "article",
            "cited_by_count": 671,
            "is_retracted": false,
            "primary_location": {"landing_page_url": "https://dl.acm.org/doi/10.1145/1571941.1572114", "source": {"display_name": "SIGIR"}},
            "best_oa_location": null,
            "open_access": {"is_oa": false, "oa_status": "closed"},
            "authorships": [{"author": {"display_name": "Gordon V. Cormack", "orcid": "https://orcid.org/0000-0002-1234-5678"}, "institutions": [{"display_name": "University of Waterloo"}]}],
            "ids": {"pmid": "https://pubmed.ncbi.nlm.nih.gov/123"}
        }))
        .unwrap();
        assert_eq!(w.title, "Reciprocal rank fusion outperforms condorcet");
        assert_eq!(w.ids.openalex.as_deref(), Some("W2148972377"));
        assert_eq!(w.ids.doi.as_deref(), Some("10.1145/1571941.1572114"));
        assert_eq!(w.ids.pmid.as_deref(), Some("123"));
        assert_eq!(w.venue.as_deref(), Some("SIGIR"));
        assert_eq!(w.oa.as_ref().unwrap().status, "closed");
        assert_eq!(
            w.authors[0].affiliation.as_deref(),
            Some("University of Waterloo")
        );
        assert_eq!(w.authors[0].orcid.as_deref(), Some("0000-0002-1234-5678"));
        assert_eq!(w.url, "https://dl.acm.org/doi/10.1145/1571941.1572114");
        // Port of test_openalex_tolerates_structural_surprises.
        for junk in [
            Value::Null,
            json!({}),
            json!({"results": "nope"}),
            json!([]),
            json!("garbage"),
        ] {
            assert!(openalex_work(&junk).is_none());
        }
    }

    #[test]
    fn s2_paper_maps_ids_and_oa() {
        let w = s2_paper(&json!({
            "paperId": "204e3073870fae3d05bcbc2f6a8e263d9b72e776",
            "title": "Attention is All you Need",
            "year": 2017,
            "publicationDate": "2017-06-12",
            "venue": "Neural Information Processing Systems",
            "citationCount": 100000,
            "influentialCitationCount": 12000,
            "openAccessPdf": {"url": "", "status": "GREEN"},
            "externalIds": {"ArXiv": "1706.03762", "DBLP": "conf/nips/VaswaniSPUJGKP17", "CorpusId": 13756489},
            "authors": [{"name": "Ashish Vaswani"}]
        }))
        .unwrap();
        assert_eq!(w.ids.arxiv.as_deref(), Some("1706.03762"));
        assert_eq!(w.ids.doi, None);
        assert_eq!(w.work_type, "preprint");
        assert_eq!(w.influential_citations, Some(12000));
        assert!(w.oa.is_none(), "an empty openAccessPdf url is not OA");
        assert_eq!(
            w.url,
            "https://www.semanticscholar.org/paper/204e3073870fae3d05bcbc2f6a8e263d9b72e776"
        );
    }

    const FEED: &str = r#"<?xml version='1.0' encoding='UTF-8'?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:arxiv="http://arxiv.org/schemas/atom">
  <entry>
    <id>http://arxiv.org/abs/1706.03762v7</id>
    <title>Attention Is All
      You Need</title>
    <summary>The dominant sequence transduction models are based on complex
    recurrent or convolutional neural networks.</summary>
    <published>2017-06-12T18:00:00Z</published>
    <link href="https://arxiv.org/abs/1706.03762v7" type="text/html"/>
    <link href="https://arxiv.org/pdf/1706.03762v7" type="application/pdf" title="pdf"/>
    <author><name>Ashish Vaswani</name></author>
    <author><name>Noam Shazeer</name></author>
    <arxiv:doi>10.5555/3295222.3295349</arxiv:doi>
  </entry>
  <entry>
    <id>http://arxiv.org/api/errors#incorrect_id_format_for_1706</id>
    <title>Error</title>
  </entry>
</feed>"#;

    /// Port of `test_academic_engines.py::test_arxiv_parses_entries_with_abs_url_not_pdf`
    /// and `test_arxiv_malformed_xml_returns_empty`.
    #[test]
    fn test_arxiv_parses_entries_with_abs_url_not_pdf() {
        let out = arxiv_feed(FEED);
        assert_eq!(out.len(), 1, "the error entry is skipped");
        let w = &out[0];
        assert_eq!(w.url, "https://arxiv.org/abs/1706.03762");
        assert_eq!(w.title, "Attention Is All You Need");
        assert_eq!(w.ids.arxiv.as_deref(), Some("1706.03762"));
        assert_eq!(w.ids.doi.as_deref(), Some("10.5555/3295222.3295349"));
        assert_eq!(w.published_date.as_deref(), Some("2017-06-12"));
        assert_eq!(w.authors.len(), 2);
        let oa = w.oa.as_ref().unwrap();
        assert_eq!(
            oa.pdf_url.as_deref(),
            Some("https://arxiv.org/pdf/1706.03762v7")
        );
        assert!(arxiv_feed("<feed><entry>").is_empty());
        assert!(arxiv_feed("").is_empty());
    }

    #[test]
    fn idconv_and_entities() {
        let v = json!({"status": "ok", "records": [{"doi": "10.1093/nar/gks1195", "pmcid": "PMC3531190", "pmid": "23193287", "requested-id": "23193287"}]});
        let ids = idconv_ids(&v).unwrap();
        assert_eq!(ids.pmcid.as_deref(), Some("PMC3531190"));
        assert_eq!(ids.pmid.as_deref(), Some("23193287"));
        assert_eq!(ids.doi.as_deref(), Some("10.1093/nar/gks1195"));
        let err = json!({"records": [{"status": "error", "errmsg": "invalid"}]});
        assert!(idconv_ids(&err).is_none());
        assert_eq!(
            clean_text("a &lt;b&gt; &#233;&#x41; &bogus; & c"),
            "a <b> éA &bogus; & c"
        );
    }

    #[test]
    fn idconv_parses_recorded_integer_pmid_fixture() {
        let text = include_str!("../tests/fixtures/pmid_lancet/pmc_idconv.json");
        let fixture = fsm_core::fixture::Fixture::from_json(text).unwrap();
        let body = fixture.exchanges[0].response.body_text.as_deref().unwrap();
        let v: Value = serde_json::from_str(body).unwrap();
        let ids = idconv_ids(&v).unwrap();
        assert_eq!(ids.pmid.as_deref(), Some("32450107"));
        assert_eq!(ids.pmcid.as_deref(), Some("PMC7255293"));
        assert_eq!(ids.doi.as_deref(), Some("10.1016/s0140-6736(20)31180-6"));
    }

    #[test]
    fn clean_text_preserves_math_inequalities_and_decoded_entities_and_clean_abstract_preserves_words()
     {
        // Math inequalities survive clean_text and clean_abstract intact.
        let med = "Patients with BMI < 25 had mortality > 10% (p < 0.01) vs BMI > 30.";
        assert_eq!(clean_text(med), med);
        assert_eq!(clean_abstract(med).as_deref(), Some(med));
        assert_eq!(clean_text("x<y and z>w"), "x<y and z>w");
        assert_eq!(clean_text("x &lt; y and z &gt; w"), "x < y and z > w");
        // Idempotent on decoded inequalities.
        assert_eq!(
            clean_text(&clean_text("x &lt; y and z &gt; w")),
            "x < y and z > w"
        );
        // Decoded literal markup survives crossref_work + merge::prepare_parsed without double-clean.
        let cr = crossref_work(&json!({
            "title": ["a &lt;b&gt; c"],
            "DOI": "10.1234/ent",
            "abstract": "<jats:p>We show a &lt;b&gt; tag and p &lt; 0.01 vs &gt; 30.</jats:p>"
        }))
        .unwrap();
        assert_eq!(cr.title, "a <b> c");
        assert_eq!(
            cr.abstract_text.as_deref(),
            Some("We show a <b> tag and p < 0.01 vs > 30.")
        );
        let prepared = crate::merge::prepare_parsed(cr, Some("crossref"));
        assert_eq!(prepared.title, "a <b> c");
        assert_eq!(
            prepared.abstract_text.as_deref(),
            Some("We show a <b> tag and p < 0.01 vs > 30.")
        );
        // arXiv XML decoded by roxmltree is never re-stripped.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>http://arxiv.org/abs/2401.00001v1</id>
    <title>Generic Vec&lt;T&gt; with p &lt; 0.01 &gt; 0</title>
    <summary>Patients with BMI &lt; 25 had mortality &gt; 10% and a &lt;b&gt; tag.</summary>
    <published>2024-01-01T00:00:00Z</published>
  </entry>
</feed>"#;
        let ax = arxiv_feed(xml).into_iter().next().unwrap();
        assert_eq!(ax.title, "Generic Vec<T> with p < 0.01 > 0");
        assert_eq!(
            ax.abstract_text.as_deref(),
            Some("Patients with BMI < 25 had mortality > 10% and a <b> tag.")
        );
        // clean_abstract never slices words starting with "Abstract".
        for prose in [
            "Abstraction layers in distributed systems reduce complexity.",
            "Abstracting hardware details improves portability.",
            "Abstracts of recent papers were analyzed.",
            "Abstract algebra studies algebraic structures.",
        ] {
            assert_eq!(clean_abstract(prose).as_deref(), Some(prose), "{prose}");
        }
        // Genuine headings ARE stripped.
        assert_eq!(
            clean_abstract("Abstract: We propose a model.").as_deref(),
            Some("We propose a model.")
        );
        assert_eq!(
            clean_abstract("ABSTRACT. We propose a model.").as_deref(),
            Some("We propose a model.")
        );
        assert_eq!(
            clean_abstract("Abstract We propose a model.").as_deref(),
            Some("We propose a model.")
        );
        assert_eq!(
            clean_abstract(
                "<jats:title>Abstract</jats:title><jats:p>Abstraction layers work.</jats:p>"
            )
            .as_deref(),
            Some("Abstraction layers work.")
        );
    }

    #[test]
    fn openalex_work_reads_ids_arxiv_on_publisher_hosted_paper() {
        let w = openalex_work(&json!({
            "id": "https://openalex.org/W2194775991",
            "doi": "https://doi.org/10.1109/CVPR.2016.90",
            "display_name": "Deep Residual Learning for Image Recognition",
            "publication_year": 2016,
            "primary_location": {"landing_page_url": "https://ieeexplore.ieee.org/document/7780459"},
            "best_oa_location": null,
            "ids": {
                "openalex": "https://openalex.org/W2194775991",
                "doi": "https://doi.org/10.1109/cvpr.2016.90",
                "arxiv": "https://arxiv.org/abs/1512.03385"
            }
        }))
        .unwrap();
        assert_eq!(w.ids.arxiv.as_deref(), Some("1512.03385"));
        assert_eq!(w.ids.doi.as_deref(), Some("10.1109/cvpr.2016.90"));
        // Stage-1 dedup merges it with an arXiv record even without title/year similarity.
        let mut ax = ScholarlyWork::new("Different subtitle on preprint", "");
        ax.ids.arxiv = Some("1512.03385".into());
        ax.year = Some(2015);
        ax.sources = vec!["arxiv".into()];
        let merged = crate::ScholarCore::merge_works(vec![w, ax]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].ids.arxiv.as_deref(), Some("1512.03385"));
        assert_eq!(merged[0].ids.doi.as_deref(), Some("10.1109/cvpr.2016.90"));
    }

    #[test]
    fn arxiv_feed_rejects_xml_entity_declarations_and_caps_fields() {
        let xxe = r#"<?xml version="1.0"?>
<!DOCTYPE feed [<!ENTITY bomb "aaaaaaaaaaaaaaaa">]>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>http://arxiv.org/abs/1706.03762v1</id>
    <title>&bomb;</title>
  </entry>
</feed>"#;
        assert!(parse_xml_safe(xxe).is_none());
        assert!(arxiv_feed(xxe).is_empty());

        let huge_summary = "a".repeat(MAX_XML_FIELD_BYTES + 1024);
        let xml = format!(
            r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><entry><id>http://arxiv.org/abs/1706.03762v1</id><title>Ok</title><summary>{huge_summary}</summary></entry></feed>"#
        );
        let w = arxiv_feed(&xml).into_iter().next().unwrap();
        assert_eq!(w.abstract_text.unwrap().len(), MAX_XML_FIELD_BYTES);
    }
}
