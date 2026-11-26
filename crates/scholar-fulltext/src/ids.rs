//! Scholarly identifier normalization and lawful URL guardrails.

use fsm_core::{FsmError, ScholarlyWork, WorkIds};
use regex::Regex;
use std::sync::LazyLock;
use url::Url;

static DOI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(10\.\d{4,9}/[^\s<>]+)").expect("doi regex"));

static ARXIV_ID_PAT: &str = r"([a-z-]+(?:\.[a-z]{2})?/\d{7}|\d{4}\.\d{4,5})(v\d+)?";

static ARXIV_DOI_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(?i)^10\.48550/arxiv\.{ARXIV_ID_PAT}$")).expect("arxiv doi regex")
});

static ARXIV_REF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)(?:(?:arxiv\.org|ar5iv\.labs\.arxiv\.org)/(?:abs|pdf|html)/|\barxiv:\s*){ARXIV_ID_PAT}"
    ))
    .expect("arxiv ref regex")
});

static ARXIV_BARE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?i)^{ARXIV_ID_PAT}$")).expect("arxiv bare regex"));

static PMCID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(PMC\d{4,10})\b").expect("pmcid regex"));

static PMC_URL_NUM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)/pmc/articles/(?:PMC)?(\d{4,10})(?:/|$)").expect("pmc url num regex")
});

static PMID_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:\bpmid\s*[:/]\s*|pubmed\.ncbi\.nlm\.nih\.gov/)(\d{4,10})\b")
        .expect("pmid regex")
});

static OPENALEX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:openalex\.org/|^)(W\d{4,12})(?:\b|$)").expect("oa regex"));

static HAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b((?:hal|tel|medihal|in2p3|cea|pasteur)-\d{6,10})(?:v\d+)?\b")
        .expect("hal regex")
});

static ZENODO_URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)zenodo\.org/records?/(\d{4,12})").expect("zenodo url regex"));

static ZENODO_DOI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^10\.5281/zenodo\.(\d{4,12})$").expect("zenodo doi regex"));

static S2_HEX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-f]{40}$").expect("s2 hex regex"));

/// Shadow-library host fragments that are strictly out of scope (SPEC §1.1).
const BLOCKED_SHADOW_HOSTS: &[&str] = &[
    "sci-hub.",
    ".sci-hub.",
    "libgen.",
    ".libgen.",
    "library.lol",
    "z-lib.",
    ".z-lib.",
    "z-library.",
    ".z-library.",
    "singlelogin.",
    "annas-archive.",
    ".annas-archive.",
    "booksc.",
    "b-ok.",
    "1lib.",
];

/// Normalized identifiers extracted from caller input or upstream metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedPaperId {
    /// Original trimmed input string.
    pub raw: String,
    /// Stable canonical identifier for reporting and cache keys.
    pub canonical_id: String,
    /// Lowercase bare DOI (`10.xxxx/...`).
    pub doi: Option<String>,
    /// Canonical base arXiv ID without version (`1706.03762` or `hep-th/9901001`).
    pub arxiv: Option<String>,
    /// arXiv ID preserving explicit `vN` suffix when the caller supplied one.
    pub arxiv_fetch: Option<String>,
    /// True when `doi` is an arXiv DataCite DOI (`10.48550/arxiv.*`).
    pub is_arxiv_doi: bool,
    /// PubMed numeric ID.
    pub pmid: Option<String>,
    /// PubMed Central ID (`PMC...`, never `PPR...`).
    pub pmcid: Option<String>,
    /// OpenAlex Work ID (`W...`).
    pub openalex: Option<String>,
    /// Semantic Scholar 40-char SHA paper ID.
    pub s2: Option<String>,
    /// HAL archive ID (`hal-...`).
    pub hal: Option<String>,
    /// Zenodo numeric record ID.
    pub zenodo: Option<String>,
    /// Known landing URL (from caller URL or metadata).
    pub landing_url: Option<String>,
    /// Hint when caller passed a `biorxiv.org` or `medrxiv.org` URL directly.
    pub biorxiv_server_hint: Option<&'static str>,
}

impl ParsedPaperId {
    /// True if at least one persistent scholarly identifier was recognized
    /// (excluding a bare publisher/repository URL).
    pub fn has_scholarly_pid(&self) -> bool {
        self.doi.is_some()
            || self.arxiv.is_some()
            || self.pmid.is_some()
            || self.pmcid.is_some()
            || self.openalex.is_some()
            || self.s2.is_some()
            || self.hal.is_some()
            || self.zenodo.is_some()
    }

    /// True if at least one persistent identifier or URL was recognized.
    pub fn has_any_id(&self) -> bool {
        self.has_scholarly_pid() || self.landing_url.is_some()
    }

    /// True when we have a non-arXiv publisher/preprint DOI suitable for
    /// PMC idconv / Unpaywall / Crossref / OpenAlex DOI lookups.
    pub fn non_arxiv_doi(&self) -> Option<&str> {
        self.doi
            .as_deref()
            .filter(|d| !d.starts_with("10.48550/arxiv."))
    }

    /// Best arXiv ID to fetch from `arxiv.org/html/{id}` / `ar5iv` / `pdf`.
    pub fn arxiv_fetch_id(&self) -> Option<&str> {
        self.arxiv_fetch.as_deref().or(self.arxiv.as_deref())
    }

    /// Merge identifiers from a cached or resolved [`ScholarlyWork`].
    pub fn merge_work(&mut self, work: &ScholarlyWork) {
        self.merge_work_ids(&work.ids);
        if self.landing_url.is_none()
            && !work.url.is_empty()
            && work.url.starts_with("http")
            && !is_blocked_shadow_url(&work.url)
        {
            self.landing_url = Some(work.url.clone());
        }
        // Check published versions for a journal DOI when we only had a preprint.
        if self.non_arxiv_doi().is_none() {
            for v in &work.versions {
                if v.kind.eq_ignore_ascii_case("published")
                    && let Some(ref raw_doi) = v.doi
                    && let Some(norm) = normalize_doi(raw_doi)
                {
                    self.doi = Some(norm);
                    self.is_arxiv_doi = false;
                    break;
                }
            }
        }
        if self.arxiv.is_none() {
            for v in &work.versions {
                if let Some(ref raw_ax) = v.arxiv
                    && let Some((base, fetch)) = normalize_arxiv_id(raw_ax)
                {
                    self.arxiv = Some(base);
                    self.arxiv_fetch = Some(fetch);
                    break;
                }
            }
        }
        self.refresh_canonical();
    }

    /// Merge identifiers from [`WorkIds`].
    pub fn merge_work_ids(&mut self, ids: &WorkIds) {
        if self.doi.is_none()
            && let Some(ref d) = ids.doi
            && let Some(norm) = normalize_doi(d)
        {
            self.is_arxiv_doi = norm.starts_with("10.48550/arxiv.");
            if self.arxiv.is_none()
                && let Some((base, fetch)) = extract_arxiv_from_doi(&norm)
            {
                self.arxiv = Some(base);
                self.arxiv_fetch = Some(fetch);
            }
            if self.zenodo.is_none() {
                self.zenodo = extract_zenodo_id_from_doi(&norm);
            }
            self.doi = Some(norm);
        }
        if self.arxiv.is_none()
            && let Some(ref a) = ids.arxiv
            && let Some((base, fetch)) = normalize_arxiv_id(a)
        {
            self.arxiv = Some(base);
            self.arxiv_fetch = Some(fetch);
        }
        if self.pmcid.is_none()
            && let Some(ref p) = ids.pmcid
            && let Some(norm) = normalize_pmcid(p)
        {
            self.pmcid = Some(norm);
        }
        if self.pmid.is_none()
            && let Some(ref p) = ids.pmid
            && let Some(norm) = normalize_pmid(p)
        {
            self.pmid = Some(norm);
        }
        if self.openalex.is_none()
            && let Some(ref o) = ids.openalex
            && let Some(norm) = normalize_openalex_id(o)
        {
            self.openalex = Some(norm);
        }
        if self.s2.is_none()
            && let Some(ref s) = ids.s2
        {
            let trimmed = s.trim().to_ascii_lowercase();
            if S2_HEX_RE.is_match(&trimmed) {
                self.s2 = Some(trimmed);
            }
        }
        self.refresh_canonical();
    }

    pub fn refresh_canonical(&mut self) {
        self.canonical_id = if let Some(ref d) = self.doi {
            d.clone()
        } else if let Some(ref a) = self.arxiv {
            format!("arXiv:{a}")
        } else if let Some(ref pmc) = self.pmcid {
            pmc.clone()
        } else if let Some(ref pmid) = self.pmid {
            format!("PMID:{pmid}")
        } else if let Some(ref oa) = self.openalex {
            oa.clone()
        } else if let Some(ref hal) = self.hal {
            hal.clone()
        } else if let Some(ref z) = self.zenodo {
            format!("10.5281/zenodo.{z}")
        } else if let Some(ref s2) = self.s2 {
            s2.clone()
        } else {
            self.raw.clone()
        };
    }

    /// Candidate lookup keys for `StoreApi::scholar_get`.
    pub fn store_lookup_keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        if let Some(ref d) = self.doi {
            keys.push(d.clone());
            keys.push(format!("doi:{d}"));
        }
        if let Some(ref a) = self.arxiv {
            keys.push(format!("arXiv:{a}"));
            keys.push(format!("arxiv:{a}"));
            keys.push(a.clone());
        }
        if let Some(ref pmc) = self.pmcid {
            keys.push(pmc.clone());
        }
        if let Some(ref pmid) = self.pmid {
            keys.push(format!("pmid:{pmid}"));
        }
        if let Some(ref oa) = self.openalex {
            keys.push(oa.clone());
        }
        if keys.is_empty() && !self.raw.is_empty() {
            keys.push(self.raw.clone());
        }
        keys
    }
}

/// Parse caller input (DOI, arXiv ID, PMCID, PMID, OpenAlex ID, HAL ID, Zenodo URL,
/// bioRxiv/medRxiv URL, publisher URL, or title) into structured identifiers.
pub fn parse_paper_id(raw: &str) -> Result<ParsedPaperId, FsmError> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(FsmError::invalid("id_or_doi must not be empty"));
    }
    if (text.starts_with("http://") || text.starts_with("https://")) && is_blocked_shadow_url(text)
    {
        return Err(FsmError::PermissionDenied(
            "shadow libraries are out of scope; only lawful open-access sources are supported"
                .to_owned(),
        ));
    }

    let mut out = ParsedPaperId {
        raw: text.to_owned(),
        ..ParsedPaperId::default()
    };

    let lower = text.to_ascii_lowercase();
    if lower.contains("biorxiv.org") {
        out.biorxiv_server_hint = Some("biorxiv");
    } else if lower.contains("medrxiv.org") {
        out.biorxiv_server_hint = Some("medrxiv");
    }

    // 1. OpenAlex URL or bare W-ID
    if lower.contains("openalex.org")
        && let Some(oa) = normalize_openalex_id(text)
    {
        out.openalex = Some(oa);
    }

    // 2. PMCID
    if let Some(pmc) = normalize_pmcid(text) {
        out.pmcid = Some(pmc);
    }

    // 3. PMID (explicit prefix or PubMed URL)
    if let Some(caps) = PMID_PREFIX_RE.captures(text)
        && let Some(m) = caps.get(1)
    {
        out.pmid = Some(m.as_str().to_owned());
    }

    // 4. arXiv URL, arXiv: prefix, or bare arXiv ID
    if let Some((base, fetch)) = normalize_arxiv_id(text) {
        out.arxiv = Some(base);
        out.arxiv_fetch = Some(fetch);
    }

    // 5. DOI (including doi.org URLs, bioRxiv content URLs, DataCite arXiv DOIs)
    if let Some(doi) = normalize_doi(text) {
        // Strip bioRxiv/medRxiv version or .full/.pdf suffix attached to URL DOI.
        let cleaned_doi = clean_biorxiv_url_doi(&doi, out.biorxiv_server_hint.is_some());
        if let Some((base, fetch)) = extract_arxiv_from_doi(&cleaned_doi) {
            out.is_arxiv_doi = true;
            if out.arxiv.is_none() {
                out.arxiv = Some(base);
                out.arxiv_fetch = Some(fetch);
            }
        }
        if let Some(zid) = extract_zenodo_id_from_doi(&cleaned_doi) {
            out.zenodo = Some(zid);
        }
        out.doi = Some(cleaned_doi);
    }

    // 6. HAL ID
    if let Some(caps) = HAL_RE.captures(text)
        && let Some(m) = caps.get(1)
    {
        out.hal = Some(m.as_str().to_ascii_lowercase());
    }

    // 7. Zenodo URL
    if out.zenodo.is_none()
        && let Some(caps) = ZENODO_URL_RE.captures(text)
        && let Some(m) = caps.get(1)
    {
        out.zenodo = Some(m.as_str().to_owned());
    }

    // 8. Bare OpenAlex W-ID or S2 40-char hex
    if !out.has_any_id() {
        if let Some(oa) = normalize_openalex_id(text) {
            out.openalex = Some(oa);
        } else if S2_HEX_RE.is_match(&lower) {
            out.s2 = Some(lower);
        }
    }

    // 9. Preserve http(s) URL as landing_url when given directly
    if (text.starts_with("http://") || text.starts_with("https://")) && Url::parse(text).is_ok() {
        out.landing_url = Some(text.to_owned());
    }

    out.refresh_canonical();
    Ok(out)
}

fn clean_biorxiv_url_doi(doi: &str, is_rxiv_url: bool) -> String {
    if !is_rxiv_url && !is_biorxiv_doi(doi) {
        return doi.to_owned();
    }
    let mut s = doi;
    for suffix in [
        ".full.pdf",
        ".full.txt",
        ".full",
        ".abstract",
        ".source.xml",
    ] {
        if let Some(stripped) = s.strip_suffix(suffix) {
            s = stripped;
        }
    }
    // Strip trailing `v1`, `v2`, ... version marker on bioRxiv/medRxiv URLs
    let prefix_len = if s.starts_with("10.64898/") {
        "10.64898/".len()
    } else {
        "10.1101/".len()
    };
    if let Some(idx) = s.rfind('v') {
        let tail = &s[idx + 1..];
        if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) && idx > prefix_len {
            s = &s[..idx];
        }
    }
    s.to_owned()
}

/// Normalize any DOI string, `doi:` prefix, or `doi.org/` URL into lowercase bare `10.xxxx/...`.
pub fn normalize_doi(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = trimmed
        .strip_prefix("https://doi.org/")
        .or_else(|| trimmed.strip_prefix("http://doi.org/"))
        .or_else(|| trimmed.strip_prefix("https://dx.doi.org/"))
        .or_else(|| trimmed.strip_prefix("http://dx.doi.org/"))
        .unwrap_or(trimmed);
    let candidate = if candidate.len() >= 4 && candidate[..4].eq_ignore_ascii_case("doi:") {
        candidate[4..].trim()
    } else {
        candidate
    };
    let caps = DOI_RE.captures(candidate)?;
    let matched = caps.get(1)?.as_str();
    let cleaned = matched
        .split(['?', '#'])
        .next()
        .unwrap_or(matched)
        .trim_end_matches(['.', ',', ';', ')', ']', '"', '\'']);
    if cleaned.len() < 8 || !cleaned.contains('/') {
        return None;
    }
    Some(cleaned.to_ascii_lowercase())
}

/// Normalize an arXiv reference, URL, or bare ID into `(canonical_base, fetch_id)`.
pub fn normalize_arxiv_id(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim().trim_end_matches(".pdf");
    if let Some(caps) = ARXIV_REF_RE.captures(trimmed) {
        let base = caps.get(1)?.as_str().to_ascii_lowercase();
        let ver = caps.get(2).map_or("", |m| m.as_str()).to_ascii_lowercase();
        let fetch = format!("{base}{ver}");
        return Some((base, fetch));
    }
    if let Some(caps) = ARXIV_BARE_RE.captures(trimmed) {
        let base = caps.get(1)?.as_str().to_ascii_lowercase();
        let ver = caps.get(2).map_or("", |m| m.as_str()).to_ascii_lowercase();
        let fetch = format!("{base}{ver}");
        return Some((base, fetch));
    }
    None
}

/// Extract `(canonical_base, fetch_id)` from `10.48550/arxiv.<id>`.
pub fn extract_arxiv_from_doi(doi: &str) -> Option<(String, String)> {
    let caps = ARXIV_DOI_RE.captures(doi.trim())?;
    let base = caps.get(1)?.as_str().to_ascii_lowercase();
    let ver = caps.get(2).map_or("", |m| m.as_str()).to_ascii_lowercase();
    Some((base.clone(), format!("{base}{ver}")))
}

/// Normalize a PubMed Central ID (`PMC123456`, including OpenAlex's
/// `https://www.ncbi.nlm.nih.gov/pmc/articles/4221854` form). Explicitly rejects
/// `PPR*` preprint IDs because Europe PMC `fullTextXML` returns HTTP 500 on `PPR*` (`academic.md` L447).
pub fn normalize_pmcid(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(caps) = PMCID_RE.captures(trimmed) {
        return Some(caps.get(1)?.as_str().to_ascii_uppercase());
    }
    if let Some(caps) = PMC_URL_NUM_RE.captures(trimmed) {
        return Some(format!("PMC{}", caps.get(1)?.as_str()));
    }
    None
}

/// Normalize a numeric PubMed ID.
pub fn normalize_pmid(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let stripped = if trimmed.len() >= 5 && trimmed[..5].eq_ignore_ascii_case("pmid:") {
        trimmed[5..].trim()
    } else {
        trimmed
    };
    if (4..=10).contains(&stripped.len()) && stripped.bytes().all(|b| b.is_ascii_digit()) {
        Some(stripped.to_owned())
    } else {
        None
    }
}

/// Normalize an OpenAlex Work ID (`W1234567890`).
pub fn normalize_openalex_id(raw: &str) -> Option<String> {
    let caps = OPENALEX_RE.captures(raw.trim())?;
    Some(caps.get(1)?.as_str().to_ascii_uppercase())
}

/// Extract Zenodo numeric record ID from `10.5281/zenodo.<id>`.
pub fn extract_zenodo_id_from_doi(doi: &str) -> Option<String> {
    let caps = ZENODO_DOI_RE.captures(doi.trim())?;
    Some(caps.get(1)?.as_str().to_owned())
}

/// True if `url` points to a known shadow-library domain (strictly forbidden).
pub fn is_blocked_shadow_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    BLOCKED_SHADOW_HOSTS
        .iter()
        .any(|pat| host.starts_with(pat) || host.contains(pat))
}

/// True if `url` points to OpenAlex's paid `/content` PDF proxy ($10/1k; `academic.md` L451).
pub fn is_paid_openalex_content_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    parsed
        .host_str()
        .is_some_and(|h| h.eq_ignore_ascii_case("api.openalex.org"))
        && parsed.path().starts_with("/content")
}

/// True if `url` is a disallowed arXiv `/e-print` or `/src` path (`academic.md` L90).
pub fn is_disallowed_arxiv_source_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    parsed
        .host_str()
        .is_some_and(|h| h.eq_ignore_ascii_case("arxiv.org") || h.ends_with(".arxiv.org"))
        && (parsed.path().starts_with("/e-print") || parsed.path().starts_with("/src"))
}

/// True if `url` is lawful to fetch: http(s), not shadow-library, not paid OpenAlex proxy,
/// not arXiv disallowed `/e-print` or `/src`.
pub fn is_lawful_candidate_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    matches!(parsed.scheme(), "http" | "https")
        && !is_blocked_shadow_url(url)
        && !is_paid_openalex_content_url(url)
        && !is_disallowed_arxiv_source_url(url)
}

pub fn is_biorxiv_doi(doi: &str) -> bool {
    doi.starts_with("10.1101/") || doi.starts_with("10.64898/")
}

pub fn is_plos_doi(doi: &str) -> bool {
    doi.starts_with("10.1371/")
}

/// Map PLOS DOI prefix to its `journals.plos.org/{slug}` path segment.
pub fn plos_journal_slug(doi: &str) -> &'static str {
    if doi.starts_with("10.1371/journal.pbio.") {
        "plosbiology"
    } else if doi.starts_with("10.1371/journal.pmed.") {
        "plosmedicine"
    } else if doi.starts_with("10.1371/journal.pgen.") {
        "plosgenetics"
    } else if doi.starts_with("10.1371/journal.pcbi.") {
        "ploscompbiol"
    } else if doi.starts_with("10.1371/journal.ppat.") {
        "plospathogens"
    } else if doi.starts_with("10.1371/journal.pntd.") {
        "plosntds"
    } else {
        "plosone"
    }
}

/// Known Springer Nature / Nature Portfolio / BMC DOI prefixes for Step 6b.
pub fn is_springer_nature_doi(doi: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "10.1007/", "10.1038/", "10.1186/", "10.1140/", "10.1057/", "10.1365/", "10.1023/",
        "10.4081/", "10.1557/", "10.1617/",
    ];
    PREFIXES.iter().any(|p| doi.starts_with(p))
}
