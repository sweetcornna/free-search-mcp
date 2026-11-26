//! Citation formatting (`ScholarGraph::cite`, `SPEC.md` §6, `academic.md` §12).
//!
//! - Offline formatting for `CiteInput::Work` across all 6 `CiteStyle` values:
//!   `Bibtex`, `Csl` (CSL-JSON), `Apa`, `Ieee`, `Chicago`, `Vancouver` via
//!   `hayagriva 0.10` (`ArchivedStyle` + bundled locales, plain output without
//!   ANSI escapes).
//! - For `CiteInput::Id`, tries DOI content negotiation (`https://doi.org/{doi}`
//!   with `Accept: application/x-bibtex` or `application/vnd.citationstyles.csl+json`),
//!   formatting negotiated CSL-JSON locally through `hayagriva`, and falls back
//!   to cached/resolved `ScholarlyWork` metadata when DOI content negotiation is
//!   unavailable or the input is an arXiv/OpenAlex/PMID/title identifier.

use crate::graph::{
    encode_doi_path, get_json_with_status, net_cx_from_call, openalex_url, parse_openalex_node,
};
use crate::ids::{TITLE_MATCH_MIN, title_score};
use fsm_core::{
    CallCx, CiteInput, CiteStyle, FsmError, HttpRequest, ScholarlyWork, SourceDeps,
    TransportProfile,
};
use fsm_scholar::ScholarCore;
use fsm_scholar::ids::{Pid, classify as classify_scholar_pid};
use hayagriva::archive::{ArchivedStyle, locales};
use hayagriva::citationberg::{IndependentStyle, Locale, Style, json as csl_json};
use hayagriva::types::{Date, EntryType, FormatString, Person, QualifiedUrl};
use hayagriva::{BibliographyDriver, BibliographyRequest, CitationItem, CitationRequest, Entry};
use serde_json::{Map, Value, json};
use std::str::FromStr;
use std::sync::LazyLock;

const DOI_ORG: &str = "https://doi.org";
const OPENALEX_WORKS: &str = "https://api.openalex.org/works";

static ARCHIVE_LOCALES: LazyLock<Vec<Locale>> = LazyLock::new(locales);
static STYLE_APA: LazyLock<IndependentStyle> =
    LazyLock::new(|| load_independent(ArchivedStyle::AmericanPsychologicalAssociation));
static STYLE_IEEE: LazyLock<IndependentStyle> =
    LazyLock::new(|| load_independent(ArchivedStyle::InstituteOfElectricalAndElectronicsEngineers));
static STYLE_CHICAGO: LazyLock<IndependentStyle> =
    LazyLock::new(|| load_independent(ArchivedStyle::ChicagoAuthorDate));
static STYLE_VANCOUVER: LazyLock<IndependentStyle> =
    LazyLock::new(|| load_independent(ArchivedStyle::SageVancouver));

fn load_independent(archived: ArchivedStyle) -> IndependentStyle {
    match archived.get() {
        Style::Independent(s) => s,
        Style::Dependent(_) => {
            unreachable!("builtin style {archived:?} must be independent")
        }
    }
}

fn csl_style_for(style: CiteStyle) -> Option<&'static IndependentStyle> {
    match style {
        CiteStyle::Apa => Some(&STYLE_APA),
        CiteStyle::Ieee => Some(&STYLE_IEEE),
        CiteStyle::Chicago => Some(&STYLE_CHICAGO),
        CiteStyle::Vancouver => Some(&STYLE_VANCOUVER),
        CiteStyle::Bibtex | CiteStyle::Csl => None,
        _ => None,
    }
}

/// Core `ScholarGraph::cite` implementation.
pub async fn run_cite(
    core: &ScholarCore,
    deps: &SourceDeps,
    input: CiteInput,
    style: CiteStyle,
    cx: &CallCx,
) -> Result<String, FsmError> {
    if cx.cancel.is_cancelled() {
        return Err(FsmError::Timeout(0));
    }
    match input {
        CiteInput::Work(work) => format_work_offline(&work, style),
        CiteInput::Id(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err(FsmError::invalid("citation identifier must not be empty"));
            }
            cite_by_id(core, deps, trimmed, style, cx).await
        }
        _ => Err(FsmError::invalid("unsupported CiteInput variant")),
    }
}

async fn cite_by_id(
    core: &ScholarCore,
    deps: &SourceDeps,
    raw: &str,
    style: CiteStyle,
    cx: &CallCx,
) -> Result<String, FsmError> {
    let pid = classify_scholar_pid(raw);

    // Direct DOI: prefer DOI Content Negotiation first for rich publisher metadata.
    if let Pid::Doi(ref doi) = pid
        && let Some(formatted) = try_doi_content_negotiation(deps, doi, style, cx).await?
    {
        return Ok(formatted);
    }

    // Resolve to ScholarlyWork via ScholarCore::work (shared cache, arXiv crosswalk,
    // S2/OpenAlex 429 bench, and consistent ID classification).
    let Some(work) = resolve_work_for_cite(core, deps, raw, &pid, cx).await? else {
        return Err(FsmError::invalid(format!(
            "could not resolve paper '{raw}' for citation"
        )));
    };

    // If a non-DOI input resolved to a published DOI (not just an arXiv DataCite DOI),
    // try DOI Content Negotiation before offline formatting.
    if !matches!(pid, Pid::Doi(_))
        && let Some(ref doi) = work.ids.doi
        && !doi.starts_with("10.48550/arxiv.")
        && let Some(formatted) = try_doi_content_negotiation(deps, doi, style, cx).await?
    {
        return Ok(formatted);
    }

    format_work_offline(&work, style)
}

/// Perform DOI Content Negotiation (`https://doi.org/{doi}`) and render locally
/// with `hayagriva` for styled formats (`Apa`, `Ieee`, `Chicago`, `Vancouver`),
/// or return sanitized CSL-JSON (`Csl`) / BibTeX (`Bibtex`).
async fn try_doi_content_negotiation(
    deps: &SourceDeps,
    doi: &str,
    style: CiteStyle,
    cx: &CallCx,
) -> Result<Option<String>, FsmError> {
    let url = format!("{DOI_ORG}/{}", encode_doi_path(doi));
    let accept = match style {
        CiteStyle::Bibtex => "application/x-bibtex",
        CiteStyle::Csl
        | CiteStyle::Apa
        | CiteStyle::Ieee
        | CiteStyle::Chicago
        | CiteStyle::Vancouver => "application/vnd.citationstyles.csl+json",
        _ => "application/vnd.citationstyles.csl+json",
    };

    let req = HttpRequest::get(url)
        .header("Accept", accept)
        .for_source("doi_cn");
    let ncx = net_cx_from_call(cx, deps);
    let resp = match deps.net.send(req, TransportProfile::Plain, &ncx).await {
        Ok(r) => r,
        Err(FsmError::Timeout(0)) if cx.cancel.is_cancelled() => return Err(FsmError::Timeout(0)),
        Err(e) => {
            tracing::debug!(error = %e, doi, "DOI content negotiation failed");
            return Ok(None);
        }
    };
    if !resp.is_success() || resp.body.is_empty() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&resp.body);
    let trimmed = text.trim();

    match style {
        CiteStyle::Bibtex => {
            if trimmed.starts_with('@') {
                Ok(Some(trimmed.to_owned()))
            } else {
                Ok(None)
            }
        }
        CiteStyle::Csl => {
            let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
                return Ok(None);
            };
            // M8: strip bloated Crossref arrays (`reference`, `link`, `license`, `score`, etc.)
            // and verify the cleaned payload conforms to `citationberg::json::Item`.
            let Some(clean) = sanitize_csl_json_value(&val, Some(doi)) else {
                return Ok(None);
            };
            if serde_json::from_value::<csl_json::Item>(clean.clone()).is_err() {
                return Ok(None);
            }
            Ok(serde_json::to_string_pretty(&clean).ok())
        }
        _ => {
            let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
                return Ok(None);
            };
            let Some(item) = sanitize_csl_json_item(&val, Some(doi)) else {
                return Ok(None);
            };
            let Some(csl_style) = csl_style_for(style) else {
                return Ok(None);
            };
            Ok(render_csl_item_plain(&item, csl_style))
        }
    }
}

async fn resolve_work_for_cite(
    core: &ScholarCore,
    deps: &SourceDeps,
    raw: &str,
    pid: &Pid,
    cx: &CallCx,
) -> Result<Option<ScholarlyWork>, FsmError> {
    match core.work(raw, cx).await {
        Ok(Some(work)) => Ok(Some(work)),
        Ok(None) => Ok(None),
        Err(FsmError::Timeout(0)) if cx.cancel.is_cancelled() => Err(FsmError::Timeout(0)),
        Err(e) => {
            tracing::debug!(error = %e, raw, "ScholarCore::work unavailable for cite");
            // When `Registry` has no search sources (e.g. minimal unit-test harnesses),
            // title lookups via `ScholarCore::work` return `Upstream`; fall back to
            // a direct OpenAlex title lookup. For all typed identifiers (DOI, arXiv,
            // PMID, PMCID, OpenAlex, S2), propagate `e` per R2 instead of converting
            // upstream outages into `InvalidInput`.
            if let Pid::Title(title) = pid {
                let url = openalex_url(
                    OPENALEX_WORKS,
                    &[("search", title), ("per-page", "5")],
                    deps,
                );
                let (status, payload) =
                    get_json_with_status(Some(core), deps, &url, "openalex", cx).await?;
                if let Some(val) = payload {
                    let matched = val
                        .get("results")
                        .and_then(Value::as_array)
                        .and_then(|arr| {
                            arr.iter().find(|item| {
                                item.get("display_name")
                                    .and_then(Value::as_str)
                                    .is_some_and(|dn| title_score(raw, dn) >= TITLE_MATCH_MIN)
                            })
                        });
                    return Ok(matched.map(parse_openalex_node));
                }
                if status == Some(404) {
                    return Ok(None);
                }
            }
            Err(e)
        }
    }
}

/// Format a [`ScholarlyWork`] completely offline in the requested [`CiteStyle`].
pub fn format_work_offline(work: &ScholarlyWork, style: CiteStyle) -> Result<String, FsmError> {
    if work.title.trim().is_empty() && work.ids.is_empty() {
        return Err(FsmError::invalid(
            "cannot format citation: work has no title or identifiers",
        ));
    }
    match style {
        CiteStyle::Bibtex => Ok(format_bibtex(work)),
        CiteStyle::Csl => {
            let csl = work_to_csl_json(work);
            serde_json::to_string_pretty(&csl)
                .map_err(|e| FsmError::internal("cite_csl", e.to_string()))
        }
        _ => {
            let Some(csl_style) = csl_style_for(style) else {
                return Err(FsmError::invalid("unsupported citation style"));
            };
            let entry = work_to_hayagriva_entry(work);
            render_entry_plain(&entry, csl_style).ok_or_else(|| {
                FsmError::internal("cite_hayagriva", "hayagriva produced empty bibliography")
            })
        }
    }
}

/// Render a `hayagriva::Entry` as plain text (using `{:#}` so zero ANSI VT100
/// escape sequences are emitted).
fn render_entry_plain(entry: &Entry, style: &IndependentStyle) -> Option<String> {
    let mut driver = BibliographyDriver::new();
    driver.citation(CitationRequest::from_items(
        vec![CitationItem::with_entry(entry)],
        style,
        &ARCHIVE_LOCALES,
    ));
    let rendered = driver.finish(BibliographyRequest::new(style, None, &ARCHIVE_LOCALES));
    extract_plain_bibliography(rendered)
}

fn render_csl_item_plain(item: &csl_json::Item, style: &IndependentStyle) -> Option<String> {
    let mut driver = BibliographyDriver::new();
    driver.citation(CitationRequest::from_items(
        vec![CitationItem::with_entry(item)],
        style,
        &ARCHIVE_LOCALES,
    ));
    let rendered = driver.finish(BibliographyRequest::new(style, None, &ARCHIVE_LOCALES));
    extract_plain_bibliography(rendered)
}

fn extract_plain_bibliography(rendered: hayagriva::Rendered) -> Option<String> {
    if let Some(bib) = rendered.bibliography
        && let Some(row) = bib.items.into_iter().next()
    {
        let content = format!("{:#}", row.content);
        let full = if let Some(prefix) = row.first_field {
            let p = format!("{prefix:#}");
            if p.trim().is_empty() {
                content
            } else {
                format!("{} {}", p.trim(), content.trim())
            }
        } else {
            content
        };
        let cleaned = full.trim().to_owned();
        if !cleaned.is_empty() {
            return Some(cleaned);
        }
    }
    rendered
        .citations
        .into_iter()
        .next()
        .map(|c| format!("{:#}", c.citation).trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Convert a [`ScholarlyWork`] to a typed [`hayagriva::Entry`].
fn work_to_hayagriva_entry(work: &ScholarlyWork) -> Entry {
    let key = make_cite_key(work);
    let (entry_type, parent_type) = map_hayagriva_types(&work.work_type, work.venue.is_some());
    let mut entry = Entry::new(&key, entry_type);

    if !work.title.trim().is_empty() {
        entry.set_title(FormatString::with_value(work.title.trim()));
    }

    let persons: Vec<Person> = work
        .authors
        .iter()
        .filter_map(|a| parse_person_name(&a.name))
        .collect();
    if !persons.is_empty() {
        entry.set_authors(persons);
    }

    if let Some(date) = parse_work_date(work) {
        entry.set_date(date);
    }

    if let Some(ref doi) = work.ids.doi {
        entry.set_doi(doi.clone());
    }
    if let Some(ref arxiv) = work.ids.arxiv {
        entry.set_arxiv(arxiv.clone());
    }
    if let Some(ref pmid) = work.ids.pmid {
        entry.set_pmid(pmid.clone());
    }
    if let Some(ref pmcid) = work.ids.pmcid {
        entry.set_pmcid(pmcid.clone());
    }
    if !work.url.is_empty()
        && let Ok(qurl) = QualifiedUrl::from_str(&work.url)
    {
        entry.set_url(qurl);
    }

    if let Some(ref venue) = work.venue
        && !venue.trim().is_empty()
    {
        let mut parent = Entry::new(&format!("{key}-venue"), parent_type);
        parent.set_title(FormatString::with_value(venue.trim()));
        entry.set_parents(vec![parent]);
    }

    entry
}

fn map_hayagriva_types(work_type: &str, has_venue: bool) -> (EntryType, EntryType) {
    match work_type.trim().to_ascii_lowercase().as_str() {
        "inproceedings" | "proceedings-article" | "paper-conference" | "conference" => {
            (EntryType::Article, EntryType::Proceedings)
        }
        "book" | "monograph" | "edited-book" => (EntryType::Book, EntryType::Misc),
        "chapter" | "book-chapter" | "incollection" => (EntryType::Chapter, EntryType::Book),
        "thesis" | "phdthesis" | "dissertation" => (EntryType::Thesis, EntryType::Misc),
        "report" | "techreport" => (EntryType::Report, EntryType::Misc),
        "dataset" | "software" => (EntryType::Misc, EntryType::Misc),
        "preprint" | "posted-content" => {
            if has_venue {
                (EntryType::Article, EntryType::Periodical)
            } else {
                (EntryType::Misc, EntryType::Misc)
            }
        }
        _ => (EntryType::Article, EntryType::Periodical),
    }
}

fn parse_person_name(raw: &str) -> Option<Person> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains(',')
        && let Ok(p) = Person::from_str(trimmed)
    {
        return Some(p);
    }
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    match parts.as_slice() {
        [] => None,
        [single] => Some(Person {
            name: (*single).to_owned(),
            given_name: None,
            prefix: None,
            suffix: None,
            comma_suffix: false,
            alias: None,
        }),
        _ => {
            const PARTICLES: &[&str] = &[
                "van", "von", "de", "del", "della", "der", "den", "di", "du", "la", "le", "st",
                "st.", "ter",
            ];
            let last_idx = parts.len() - 1;
            let mut prefix_start = last_idx;
            while prefix_start > 1
                && PARTICLES.contains(&parts[prefix_start - 1].to_ascii_lowercase().as_str())
            {
                prefix_start -= 1;
            }
            let given = parts[..prefix_start].join(" ");
            let prefix = if prefix_start < last_idx {
                Some(parts[prefix_start..last_idx].join(" "))
            } else {
                None
            };
            Some(Person {
                name: parts[last_idx].to_owned(),
                given_name: (!given.is_empty()).then_some(given),
                prefix,
                suffix: None,
                comma_suffix: false,
                alias: None,
            })
        }
    }
}

fn parse_work_date(work: &ScholarlyWork) -> Option<Date> {
    if let Some(ref pd) = work.published_date
        && let Ok(d) = Date::from_str(pd.trim())
    {
        return Some(d);
    }
    work.year.map(|y| Date::from_year(i32::from(y)))
}

/// Deterministic BibTeX / CSL citation key, e.g. `vaswani2017attention`.
pub fn make_cite_key(work: &ScholarlyWork) -> String {
    let surname = work
        .authors
        .first()
        .map(|a| {
            let s = a.name.trim();
            let raw = if let Some((last, _)) = s.split_once(',') {
                last
            } else {
                s.split_whitespace().last().unwrap_or("work")
            };
            ascii_slug(raw)
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "work".to_owned());

    let year = work
        .year
        .map(|y| y.to_string())
        .or_else(|| {
            work.published_date
                .as_deref()
                .and_then(|d| d.get(..4))
                .filter(|y| y.chars().all(|c| c.is_ascii_digit()))
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "nd".to_owned());

    const STOP: &[&str] = &["a", "an", "the", "on", "of", "in", "for", "to", "with"];
    let title_word = work
        .title
        .split(|c: char| !c.is_alphanumeric())
        .map(ascii_slug)
        .find(|w| w.len() >= 2 && !STOP.contains(&w.as_str()))
        .unwrap_or_default();

    format!("{surname}{year}{title_word}")
}

fn ascii_slug(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn escape_bibtex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str(r"\&"),
            '%' => out.push_str(r"\%"),
            '#' => out.push_str(r"\#"),
            '_' => out.push_str(r"\_"),
            '{' => out.push_str(r"\{"),
            '}' => out.push_str(r"\}"),
            _ => out.push(ch),
        }
    }
    out
}

/// Render a [`ScholarlyWork`] as a clean BibTeX entry offline.
pub fn format_bibtex(work: &ScholarlyWork) -> String {
    let key = make_cite_key(work);
    let wt = work.work_type.trim().to_ascii_lowercase();
    let (bib_type, venue_field) = match wt.as_str() {
        "inproceedings" | "proceedings-article" | "paper-conference" | "conference" => {
            ("inproceedings", "booktitle")
        }
        "book" | "monograph" | "edited-book" => ("book", "publisher"),
        "chapter" | "book-chapter" | "incollection" => ("incollection", "booktitle"),
        "thesis" | "phdthesis" | "dissertation" => ("phdthesis", "school"),
        "report" | "techreport" => ("techreport", "institution"),
        "preprint" | "posted-content" | "dataset" | "software" => ("misc", "howpublished"),
        _ => {
            if work.venue.is_some() {
                ("article", "journal")
            } else {
                ("misc", "howpublished")
            }
        }
    };

    let mut fields: Vec<(&str, String)> = Vec::new();
    if !work.title.trim().is_empty() {
        fields.push(("title", format!("{{{}}}", escape_bibtex(work.title.trim()))));
    }
    if !work.authors.is_empty() {
        let joined = work
            .authors
            .iter()
            .map(|a| escape_bibtex(a.name.trim()))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" and ");
        if !joined.is_empty() {
            fields.push(("author", joined));
        }
    }
    if let Some(ref venue) = work.venue
        && !venue.trim().is_empty()
    {
        fields.push((venue_field, escape_bibtex(venue.trim())));
    }
    if let Some(year) = work.year {
        fields.push(("year", year.to_string()));
    } else if let Some(ref pd) = work.published_date
        && let Some(y) = pd.get(..4)
        && y.chars().all(|c| c.is_ascii_digit())
    {
        fields.push(("year", y.to_owned()));
    }
    if let Some(ref doi) = work.ids.doi {
        fields.push(("doi", doi.clone()));
    }
    if let Some(ref arxiv) = work.ids.arxiv {
        fields.push(("eprint", arxiv.clone()));
        fields.push(("archivePrefix", "arXiv".to_owned()));
    }
    if !work.url.trim().is_empty() {
        fields.push(("url", work.url.trim().to_owned()));
    }

    let mut out = format!("@{bib_type}{{{key},\n");
    for (i, (k, v)) in fields.iter().enumerate() {
        let comma = if i + 1 < fields.len() { "," } else { "" };
        out.push_str(&format!("  {k} = {{{v}}}{comma}\n"));
    }
    out.push('}');
    out
}

/// Normalize Crossref / BibTeX / ScholarlyWork type tags into standard CSL 1.0.2
/// item types recognized by `hayagriva` / `citationberg::taxonomy::Kind` (B3).
pub fn normalize_csl_type(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "journal-article" | "article-journal" => "article-journal".to_owned(),
        "proceedings-article" | "inproceedings" | "conference" | "paper-conference" => {
            "paper-conference".to_owned()
        }
        "book-chapter" | "book-part" | "book-section" | "incollection" | "chapter" => {
            "chapter".to_owned()
        }
        "monograph" | "edited-book" | "reference-book" | "book" => "book".to_owned(),
        "posted-content" | "preprint" => "article".to_owned(),
        "dissertation" | "phdthesis" | "thesis" => "thesis".to_owned(),
        "reference-entry" => "entry-encyclopedia".to_owned(),
        "report-component" | "techreport" | "report" => "report".to_owned(),
        "peer-review" => "review".to_owned(),
        "journal-issue" => "periodical".to_owned(),
        "" => "article-journal".to_owned(),
        other => other.to_owned(),
    }
}

/// Convert a [`ScholarlyWork`] to a canonical CSL-JSON [`Value`].
pub fn work_to_csl_json(work: &ScholarlyWork) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_owned(), Value::String(make_cite_key(work)));
    let wt = work.work_type.trim().to_ascii_lowercase();
    let csl_type = if wt.is_empty() || wt == "article" {
        "article-journal".to_owned()
    } else {
        normalize_csl_type(&wt)
    };
    obj.insert("type".to_owned(), Value::String(csl_type));
    if !work.title.trim().is_empty() {
        obj.insert(
            "title".to_owned(),
            Value::String(work.title.trim().to_owned()),
        );
    }
    if !work.authors.is_empty() {
        let authors: Vec<Value> = work
            .authors
            .iter()
            .filter_map(|a| {
                let p = parse_person_name(&a.name)?;
                let mut m = Map::new();
                m.insert("family".to_owned(), Value::String(p.name));
                if let Some(g) = p.given_name {
                    m.insert("given".to_owned(), Value::String(g));
                }
                if let Some(prefix) = p.prefix {
                    m.insert("non-dropping-particle".to_owned(), Value::String(prefix));
                }
                if let Some(suffix) = p.suffix {
                    m.insert("suffix".to_owned(), Value::String(suffix));
                }
                Some(Value::Object(m))
            })
            .collect();
        if !authors.is_empty() {
            obj.insert("author".to_owned(), Value::Array(authors));
        }
    }
    if let Some(date_parts) = work_date_parts(work) {
        obj.insert("issued".to_owned(), json!({ "date-parts": [date_parts] }));
    }
    if let Some(ref venue) = work.venue
        && !venue.trim().is_empty()
    {
        obj.insert(
            "container-title".to_owned(),
            Value::String(venue.trim().to_owned()),
        );
    }
    if let Some(ref doi) = work.ids.doi {
        obj.insert("DOI".to_owned(), Value::String(doi.clone()));
    }
    if let Some(ref pmid) = work.ids.pmid {
        obj.insert("PMID".to_owned(), Value::String(pmid.clone()));
    }
    if let Some(ref pmcid) = work.ids.pmcid {
        obj.insert("PMCID".to_owned(), Value::String(pmcid.clone()));
    }
    if !work.url.trim().is_empty() {
        obj.insert("URL".to_owned(), Value::String(work.url.trim().to_owned()));
    }
    if let Some(ref abs) = work.abstract_text
        && !abs.trim().is_empty()
    {
        obj.insert("abstract".to_owned(), Value::String(abs.trim().to_owned()));
    }
    Value::Object(obj)
}

fn work_date_parts(work: &ScholarlyWork) -> Option<Vec<i64>> {
    if let Some(ref pd) = work.published_date {
        let parts: Vec<i64> = pd
            .split('-')
            .take(3)
            .filter_map(|s| s.parse::<i64>().ok())
            .collect();
        if !parts.is_empty() {
            return Some(parts);
        }
    }
    work.year.map(|y| vec![i64::from(y)])
}

/// Sanitize real-world DOI Content Negotiation CSL-JSON (stripping bloated
/// Crossref fields like `link`, `reference`, `license`, `score` float, and
/// normalizing array `ISSN`) into a bounded canonical CSL-JSON [`Value`].
pub fn sanitize_csl_json_value(val: &Value, fallback_id: Option<&str>) -> Option<Value> {
    let map = val.as_object()?;
    const STRING_FIELDS: &[&str] = &[
        "id",
        "type",
        "title",
        "title-short",
        "container-title",
        "container-title-short",
        "collection-title",
        "volume",
        "issue",
        "page",
        "publisher",
        "publisher-place",
        "DOI",
        "URL",
        "ISSN",
        "ISBN",
        "PMID",
        "PMCID",
        "abstract",
        "edition",
        "language",
        "number",
        "genre",
        "note",
        "status",
    ];
    const NAME_FIELDS: &[&str] = &["author", "editor", "translator", "container-author"];
    const DATE_FIELDS: &[&str] = &["issued", "accessed", "original-date", "submitted"];

    let mut clean = Map::new();
    for &k in STRING_FIELDS {
        let Some(v) = map.get(k) else { continue };
        match v {
            Value::String(s) if !s.trim().is_empty() => {
                clean.insert(k.to_owned(), Value::String(s.trim().to_owned()));
            }
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    clean.insert(k.to_owned(), Value::Number(i.into()));
                }
            }
            Value::Array(arr) => {
                if let Some(first) = arr.iter().find_map(Value::as_str)
                    && !first.trim().is_empty()
                {
                    clean.insert(k.to_owned(), Value::String(first.trim().to_owned()));
                }
            }
            _ => {}
        }
    }
    for &k in NAME_FIELDS {
        let Some(arr) = map.get(k).and_then(Value::as_array) else {
            continue;
        };
        let mut names = Vec::new();
        for n in arr {
            let Some(nm) = n.as_object() else { continue };
            if let Some(lit) = nm.get("literal").and_then(Value::as_str) {
                names.push(json!({ "literal": lit }));
            } else if let Some(fam) = nm.get("family").and_then(Value::as_str) {
                let mut item = Map::new();
                item.insert("family".to_owned(), Value::String(fam.to_owned()));
                for sub in [
                    "given",
                    "non-dropping-particle",
                    "dropping-particle",
                    "suffix",
                ] {
                    if let Some(sv) = nm.get(sub).and_then(Value::as_str) {
                        item.insert(sub.to_owned(), Value::String(sv.to_owned()));
                    }
                }
                names.push(Value::Object(item));
            } else if let Some(name) = nm.get("name").and_then(Value::as_str) {
                names.push(json!({ "literal": name }));
            }
        }
        if !names.is_empty() {
            clean.insert(k.to_owned(), Value::Array(names));
        }
    }
    for &k in DATE_FIELDS {
        let Some(dv) = map.get(k).and_then(Value::as_object) else {
            continue;
        };
        if let Some(dp) = dv.get("date-parts").and_then(Value::as_array) {
            let cleaned_ranges: Vec<Value> = dp
                .iter()
                .filter_map(Value::as_array)
                .map(|r| {
                    let nums: Vec<Value> = r
                        .iter()
                        .filter_map(|x| {
                            x.as_i64()
                                .or_else(|| x.as_str().and_then(|s| s.parse::<i64>().ok()))
                        })
                        .map(|n| Value::Number(n.into()))
                        .collect();
                    Value::Array(nums)
                })
                .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()))
                .collect();
            if !cleaned_ranges.is_empty() {
                clean.insert(k.to_owned(), json!({ "date-parts": cleaned_ranges }));
            }
        } else if let Some(raw) = dv.get("raw").and_then(Value::as_str) {
            clean.insert(k.to_owned(), json!({ "raw": raw }));
        } else if let Some(lit) = dv.get("literal").and_then(Value::as_str) {
            clean.insert(k.to_owned(), json!({ "literal": lit }));
        }
    }

    if !clean.contains_key("id") {
        let id = fallback_id
            .or_else(|| clean.get("DOI").and_then(Value::as_str))
            .unwrap_or("item");
        clean.insert("id".to_owned(), Value::String(id.to_owned()));
    }
    // B3: Normalize Crossref deposit types (`journal-article`, `proceedings-article`, ...)
    // into standard CSL 1.0.2 types so hayagriva style conditionals match properly.
    let norm_type = clean
        .get("type")
        .and_then(Value::as_str)
        .map(normalize_csl_type)
        .unwrap_or_else(|| "article-journal".to_owned());
    clean.insert("type".to_owned(), Value::String(norm_type));

    Some(Value::Object(clean))
}

/// Sanitize real-world DOI Content Negotiation CSL-JSON into a typed
/// [`hayagriva::citationberg::json::Item`].
pub fn sanitize_csl_json_item(val: &Value, fallback_id: Option<&str>) -> Option<csl_json::Item> {
    let clean = sanitize_csl_json_value(val, fallback_id)?;
    serde_json::from_value(clean).ok()
}

// Keep clean dispatch without the unused generic stub.
#[cfg(test)]
mod tests {
    use super::*;
    use fsm_core::AuthorRef;

    fn sample_work() -> ScholarlyWork {
        let mut w = ScholarlyWork::new(
            "Attention Is All You Need",
            "https://arxiv.org/abs/1706.03762",
        );
        w.authors = vec![
            AuthorRef::new("Ashish Vaswani"),
            AuthorRef::new("Noam Shazeer"),
        ];
        w.year = Some(2017);
        w.venue = Some("Advances in Neural Information Processing Systems".to_owned());
        w.work_type = "inproceedings".to_owned();
        w.ids.arxiv = Some("1706.03762".to_owned());
        w.ids.doi = Some("10.48550/arxiv.1706.03762".to_owned());
        w
    }

    #[test]
    fn offline_all_six_styles_render_cleanly() {
        let w = sample_work();
        let bib = format_work_offline(&w, CiteStyle::Bibtex).unwrap();
        assert!(
            bib.starts_with("@inproceedings{vaswani2017attention,"),
            "{bib}"
        );
        assert!(bib.contains("eprint = {1706.03762}"), "{bib}");

        let csl = format_work_offline(&w, CiteStyle::Csl).unwrap();
        let csl_v: Value = serde_json::from_str(&csl).unwrap();
        assert_eq!(csl_v["title"], "Attention Is All You Need");
        assert_eq!(csl_v["issued"]["date-parts"][0][0], 2017);

        for style in [
            CiteStyle::Apa,
            CiteStyle::Ieee,
            CiteStyle::Chicago,
            CiteStyle::Vancouver,
        ] {
            let s = format_work_offline(&w, style).unwrap();
            assert!(!s.contains('\x1b'), "ANSI escape in {style:?}: {s:?}");
            assert!(s.contains("Vaswani"), "missing author in {style:?}: {s}");
            assert!(s.contains("2017"), "missing year in {style:?}: {s}");
        }
    }
}
