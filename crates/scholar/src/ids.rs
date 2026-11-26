//! Persistent identifiers: recognising what a caller pasted, and normalising
//! the ids sources report (SPEC.md §6 "Identity Resolution").
//!
//! [`classify`] is a port of `paper_graph._normalize` (DOI, DOI URL, `doi:`
//! prefix, OpenAlex id or URL, arXiv id / URL / DataCite DOI, else a title),
//! extended with PMID, PMCID and Semantic Scholar ids. An arXiv id without a
//! prefix counts only as the WHOLE input: "1706.03762" inside a sentence is a
//! number, not an identifier.

use fsm_core::WorkIds;
use regex::Regex;
use std::sync::LazyLock;

/// arXiv identifier: new style `1706.03762` or old style `hep-th/9901001`,
/// with an optional version suffix that is dropped.
const ARXIV_ID: &str = r"([a-z-]+(?:\.[a-z]{2})?/[0-9]{7}|[0-9]{4}\.[0-9]{4,5})(?:v[0-9]+)?";

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("invalid built-in regex {pattern:?}: {e}"))
}

static DOI_RE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\b(10\.[0-9]{4,9}/\S+)"));
static OPENALEX_RE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\b(W[0-9]{4,12})\b"));
static OPENALEX_FULL_RE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^W[0-9]{4,12}$"));
static ARXIV_DOI_RE: LazyLock<Regex> =
    LazyLock::new(|| re(&format!(r"(?i)^10\.48550/arxiv\.{ARXIV_ID}$")));
static ARXIV_REF_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(&format!(
        r"(?i)(?:arxiv\.org/(?:abs|pdf|html)/|\barxiv:\s*){ARXIV_ID}"
    ))
});
static ARXIV_BARE_RE: LazyLock<Regex> = LazyLock::new(|| re(&format!(r"(?i)^{ARXIV_ID}$")));
static PMCID_RE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^(?:pmcid:\s*)?PMC([0-9]{4,9})$"));
static PMCID_URL_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)(?:ncbi\.nlm\.nih\.gov|europepmc\.org)/\S*?\bPMC([0-9]{4,9})\b"));
static PMCID_EXTRACT_RE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\bPMC([0-9]+)"));
static FIRST_DIGITS_RE: LazyLock<Regex> = LazyLock::new(|| re(r"([0-9]+)"));
static PMID_RE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^pmid:?\s*([0-9]{1,9})$"));
static PMID_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)(?:pubmed\.ncbi\.nlm\.nih\.gov/|europepmc\.org/(?:abstract|article)/MED/)([0-9]{1,9})\b",
    )
});
static S2_URL_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)semanticscholar\.org/paper/(?:[^/\s]+/)?([0-9a-f]{40})\b"));
static S2_BARE_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)^(?:s2:\s*|s2id:\s*)?([0-9a-f]{40})$"));
static CORPUS_RE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^corpus\s?id:\s*([0-9]{1,12})$"));

/// What a caller-supplied identifier string is, with its normalised value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Pid {
    /// Lower-cased, resolver-free DOI (never an arXiv DataCite DOI: those are [`Pid::Arxiv`]).
    Doi(String),
    /// arXiv id without version (`1706.03762`, `hep-th/9901001`).
    Arxiv(String),
    /// PubMed id (digits).
    Pmid(String),
    /// PubMed Central id (`PMC` + digits).
    Pmcid(String),
    /// OpenAlex work id (`W` + digits).
    OpenAlex(String),
    /// Semantic Scholar paper id (40 hex chars) or `CorpusId:<n>`.
    S2(String),
    /// Anything else: treated as a title (trimmed; may be empty).
    Title(String),
}

impl Default for Pid {
    fn default() -> Self {
        Self::Title(String::new())
    }
}

impl Pid {
    /// `"doi" | "arxiv" | "pmid" | "pmcid" | "openalex" | "s2" | "title"`
    /// (the `resolvedAs` vocabulary of `CitationGraph`).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Doi(_) => "doi",
            Self::Arxiv(_) => "arxiv",
            Self::Pmid(_) => "pmid",
            Self::Pmcid(_) => "pmcid",
            Self::OpenAlex(_) => "openalex",
            Self::S2(_) => "s2",
            Self::Title(_) => "title",
        }
    }

    /// The normalised value.
    pub fn value(&self) -> &str {
        match self {
            Self::Doi(v)
            | Self::Arxiv(v)
            | Self::Pmid(v)
            | Self::Pmcid(v)
            | Self::OpenAlex(v)
            | Self::S2(v)
            | Self::Title(v) => v,
        }
    }

    pub fn is_title(&self) -> bool {
        matches!(self, Self::Title(_))
    }

    /// The id as a [`WorkIds`] (empty for a title).
    pub fn to_ids(&self) -> WorkIds {
        let mut ids = WorkIds::default();
        let v = Some(self.value().to_owned());
        match self {
            Self::Doi(_) => ids.doi = v,
            Self::Arxiv(_) => ids.arxiv = v,
            Self::Pmid(_) => ids.pmid = v,
            Self::Pmcid(_) => ids.pmcid = v,
            Self::OpenAlex(_) => ids.openalex = v,
            Self::S2(_) => ids.s2 = v,
            Self::Title(_) => {}
        }
        ids
    }

    /// `"<kind>:<value>"`, the key works are cached under; `None` for a title.
    pub fn canonical_key(&self) -> Option<String> {
        (!self.is_title()).then(|| format!("{}:{}", self.kind(), self.value()))
    }
}

/// Classify what a caller pasted (see the module docs).
pub fn classify(raw: &str) -> Pid {
    let text = raw.trim();
    if text.is_empty() {
        return Pid::Title(String::new());
    }
    if text.to_ascii_lowercase().contains("openalex.org")
        && let Some(m) = OPENALEX_RE.captures(text)
    {
        return Pid::OpenAlex(m[1].to_ascii_uppercase());
    }
    if let Some(id) = arxiv_ref(text) {
        return Pid::Arxiv(id);
    }
    if let Some(doi) = find_doi(text) {
        return match arxiv_of_doi(&doi) {
            Some(id) => Pid::Arxiv(id),
            None => Pid::Doi(doi),
        };
    }
    if OPENALEX_FULL_RE.is_match(text) {
        return Pid::OpenAlex(text.to_ascii_uppercase());
    }
    if let Some(m) = PMCID_RE
        .captures(text)
        .or_else(|| PMCID_URL_RE.captures(text))
    {
        return Pid::Pmcid(format!("PMC{}", &m[1]));
    }
    if let Some(m) = PMID_RE
        .captures(text)
        .or_else(|| PMID_URL_RE.captures(text))
    {
        let digits = m[1].trim_start_matches('0');
        if !digits.is_empty() {
            return Pid::Pmid(digits.to_owned());
        }
    }
    if let Some(m) = S2_URL_RE
        .captures(text)
        .or_else(|| S2_BARE_RE.captures(text))
    {
        return Pid::S2(m[1].to_ascii_lowercase());
    }
    if let Some(m) = CORPUS_RE.captures(text) {
        return Pid::S2(format!("CorpusId:{}", &m[1]));
    }
    Pid::Title(text.to_owned())
}

/// The arXiv id an `arxiv.org/abs|pdf|html/` URL or `arXiv:` reference
/// carries, or the whole input when it is a bare id.
fn arxiv_ref(text: &str) -> Option<String> {
    ARXIV_REF_RE
        .captures(text)
        .or_else(|| ARXIV_BARE_RE.captures(text))
        .map(|m| m[1].to_ascii_lowercase())
}

/// The first DOI in `text`: percent-decoded, URL query/fragment/trailing slash
/// (when inside an `http(s)://` URL) and trailing prose punctuation /
/// unbalanced closing brackets removed, lower-cased.
fn find_doi(text: &str) -> Option<String> {
    let decoded;
    let text = if text.contains('%') {
        decoded = percent_encoding::percent_decode_str(text)
            .decode_utf8_lossy()
            .into_owned();
        decoded.as_str()
    } else {
        text
    };
    let m = DOI_RE.captures(text)?;
    let full = m.get(1)?;
    let token_start = text[..full.start()]
        .rfind(char::is_whitespace)
        .map_or(0, |i| i + 1);
    let prefix = text[token_start..full.start()]
        .trim_start_matches(['(', '[', '{', '<', '\'', '"'])
        .to_ascii_lowercase();
    let in_http_url = prefix.starts_with("http://") || prefix.starts_with("https://");
    let raw = if in_http_url {
        let cut = full
            .as_str()
            .split(['?', '#'])
            .next()
            .unwrap_or(full.as_str());
        trim_doi_tail(cut).trim_end_matches('/')
    } else {
        trim_doi_tail(full.as_str())
    };
    let doi = trim_doi_tail(raw);
    let suffix = doi.split_once('/').map_or("", |(_, s)| s);
    (!suffix.is_empty()).then(|| doi.to_lowercase())
}

fn trim_doi_tail(doi: &str) -> &str {
    let mut s = doi;
    while let Some(last) = s.chars().last() {
        let unbalanced = |open: char, close: char| {
            last == close && s.matches(open).count() < s.matches(close).count()
        };
        if matches!(last, '.' | ',' | ';' | ':' | '\'' | '"')
            || unbalanced('(', ')')
            || unbalanced('[', ']')
            || unbalanced('{', '}')
            || unbalanced('<', '>')
        {
            s = &s[..s.len() - last.len_utf8()];
        } else {
            break;
        }
    }
    s
}

/// `Some(id)` when `doi` is the DataCite DOI arXiv mints (`10.48550/arXiv.<id>`).
pub fn arxiv_of_doi(doi: &str) -> Option<String> {
    ARXIV_DOI_RE
        .captures(doi.trim())
        .map(|m| m[1].to_ascii_lowercase())
}

/// The DataCite DOI of an arXiv id (`10.48550/arxiv.1706.03762`, lower-cased).
pub fn arxiv_doi(arxiv_id: &str) -> String {
    format!("10.48550/arxiv.{}", arxiv_id.to_ascii_lowercase())
}

/// Normalise a DOI as sources report it (`https://doi.org/10.X/Y`,
/// `doi:10.X/Y`, ...): lower-cased, resolver and prefix removed. `None`
/// when there is no DOI in it.
pub fn normalize_doi(raw: &str) -> Option<String> {
    find_doi(raw.trim())
}

/// Normalise an arXiv id as sources report it (`arXiv:1706.03762v5`,
/// `http://arxiv.org/abs/1706.03762v7`, `10.48550/arXiv.1706.03762`, bare).
pub fn normalize_arxiv(raw: &str) -> Option<String> {
    let t = raw.trim();
    arxiv_ref(t).or_else(|| find_doi(t).and_then(|d| arxiv_of_doi(&d)))
}

/// DOIs minted by preprint servers: arXiv (DataCite), bioRxiv/medRxiv
/// (`10.1101/...` and openRxiv `10.64898/`), OSF/PsyArXiv/SocArXiv/EarthArXiv,
/// Preprints.org, Research Square, SSRN, ChemRxiv, TechRxiv and Authorea.
pub fn is_preprint_doi(doi: &str) -> bool {
    static BIORXIV: LazyLock<Regex> =
        LazyLock::new(|| re(r"^10\.1101/(?:[0-9]{4}\.[0-9]{2}\.[0-9]{2}\.)?[0-9]{6,}"));
    const PREFIXES: [&str; 12] = [
        "10.48550/",
        "10.64898/",
        "10.31219/",
        "10.31234/",
        "10.31235/",
        "10.31223/",
        "10.20944/preprints",
        "10.21203/rs.",
        "10.2139/ssrn",
        "10.26434/chemrxiv",
        "10.36227/techrxiv",
        "10.22541/au.",
    ];
    let d = doi.trim().to_ascii_lowercase();
    PREFIXES.iter().any(|p| d.starts_with(p)) || BIORXIV.is_match(&d)
}

/// Normalise every id of a work in place (see the module docs). An arXiv
/// DataCite DOI moves to `arxiv`, so `doi` only ever holds a real DOI.
pub fn normalize_work_ids(ids: &mut WorkIds) {
    if let Some(arxiv) = ids.arxiv.take() {
        ids.arxiv = normalize_arxiv(&arxiv);
    }
    if let Some(d) = ids.doi.take().as_deref().and_then(normalize_doi) {
        match arxiv_of_doi(&d) {
            Some(a) => {
                if ids.arxiv.is_none() {
                    ids.arxiv = Some(a);
                }
            }
            None => ids.doi = Some(d),
        }
    }
    if let Some(pmid) = ids.pmid.take() {
        let t = pmid.trim();
        let cap = PMID_URL_RE
            .captures(t)
            .or_else(|| PMID_RE.captures(t))
            .or_else(|| FIRST_DIGITS_RE.captures(t));
        ids.pmid = cap.and_then(|m| {
            let digits = m[1].trim_start_matches('0');
            (!digits.is_empty() && digits.len() <= 9).then(|| digits.to_owned())
        });
    }
    if let Some(pmcid) = ids.pmcid.take() {
        let t = pmcid.trim();
        let cap = PMCID_EXTRACT_RE.captures(t).or_else(|| {
            t.as_bytes()
                .first()
                .is_some_and(u8::is_ascii_digit)
                .then(|| FIRST_DIGITS_RE.captures(t))
                .flatten()
        });
        ids.pmcid = cap.and_then(|m| {
            let digits = m[1].trim_start_matches('0');
            (!digits.is_empty()).then(|| format!("PMC{digits}"))
        });
    }
    if let Some(oa) = ids.openalex.take() {
        let t = oa.trim();
        let extracted = OPENALEX_RE
            .captures(t)
            .map(|m| m[1].to_ascii_uppercase())
            .filter(|w| OPENALEX_FULL_RE.is_match(w));
        ids.openalex = extracted;
    }
    if let Some(s2) = ids.s2.take() {
        let t = s2.trim();
        ids.s2 = if let Some(m) = S2_BARE_RE.captures(t).or_else(|| S2_URL_RE.captures(t)) {
            Some(m[1].to_ascii_lowercase())
        } else if let Some(m) = CORPUS_RE.captures(t) {
            Some(format!("CorpusId:{}", &m[1]))
        } else {
            (!t.is_empty()).then(|| t.to_owned())
        };
    }
    for field in [&mut ids.dblp, &mut ids.openaire] {
        if let Some(v) = field.take() {
            let v = v.trim();
            *field = (!v.is_empty()).then(|| v.to_owned());
        }
    }
}

fn id_pairs(ids: &WorkIds) -> [(&'static str, &Option<String>); 8] {
    [
        ("doi", &ids.doi),
        ("arxiv", &ids.arxiv),
        ("pmid", &ids.pmid),
        ("pmcid", &ids.pmcid),
        ("openalex", &ids.openalex),
        ("s2", &ids.s2),
        ("dblp", &ids.dblp),
        ("openaire", &ids.openaire),
    ]
}

/// The key a merged work is cached under: the first of DOI, arXiv, PMID,
/// PMCID, OpenAlex, S2, DBLP, OpenAIRE, as `"<kind>:<value>"`.
pub fn canonical_key(ids: &WorkIds) -> Option<String> {
    id_pairs(ids)
        .iter()
        .find_map(|(k, v)| v.as_ref().map(|v| format!("{k}:{v}")))
}

/// Every exact-identity key of a work (stage 1 of the dedup).
pub(crate) fn identity_keys(ids: &WorkIds) -> Vec<String> {
    id_pairs(ids)
        .iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}:{v}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> (&'static str, String) {
        let p = classify(s);
        (p.kind(), p.value().to_owned())
    }

    fn pair(kind: &'static str, v: &str) -> (&'static str, String) {
        (kind, v.to_owned())
    }

    /// Port of `test_paper_graph.py::test_normalize_classifies_the_identifier`.
    #[test]
    fn test_normalize_classifies_the_identifier() {
        let doi = "10.1145/1571941.1572114";
        assert_eq!(c("10.1145/1571941.1572114"), pair("doi", doi));
        assert_eq!(
            c("https://doi.org/10.1145/1571941.1572114"),
            pair("doi", doi)
        );
        assert_eq!(c("doi:10.1145/1571941.1572114"), pair("doi", doi));
        // Pasted out of prose, trailing punctuation and all.
        assert_eq!(c("(10.1145/1571941.1572114)."), pair("doi", doi));
        assert_eq!(c("W2148972377"), pair("openalex", "W2148972377"));
        assert_eq!(
            c("https://openalex.org/W2148972377"),
            pair("openalex", "W2148972377")
        );
        assert_eq!(
            c("attention is all you need"),
            pair("title", "attention is all you need")
        );
        assert_eq!(c(""), pair("title", ""));
    }

    /// Port of `test_paper_graph.py::test_arxiv_identifiers_are_recognised`.
    #[test]
    fn test_arxiv_identifiers_are_recognised() {
        let cases = [
            ("10.48550/arXiv.1706.03762", pair("arxiv", "1706.03762")),
            (
                "https://doi.org/10.48550/ARXIV.1706.03762",
                pair("arxiv", "1706.03762"),
            ),
            (
                "https://arxiv.org/abs/1706.03762v7",
                pair("arxiv", "1706.03762"),
            ),
            (
                "https://arxiv.org/pdf/1706.03762",
                pair("arxiv", "1706.03762"),
            ),
            ("arXiv:1706.03762", pair("arxiv", "1706.03762")),
            ("arXiv:hep-th/9901001v2", pair("arxiv", "hep-th/9901001")),
            // The form people actually paste. Only when it is the whole input.
            ("1706.03762", pair("arxiv", "1706.03762")),
            ("2401.12345v3", pair("arxiv", "2401.12345")),
            ("hep-th/9901001", pair("arxiv", "hep-th/9901001")),
            (
                "GPT-4 scored 1706.0376 on it",
                pair("title", "GPT-4 scored 1706.0376 on it"),
            ),
            (
                "Results for 1706.03762 tokens",
                pair("title", "Results for 1706.03762 tokens"),
            ),
            // Everything else is classified exactly as before.
            (
                "10.1145/1571941.1572114",
                pair("doi", "10.1145/1571941.1572114"),
            ),
            ("W2741809807", pair("openalex", "W2741809807")),
            (
                "Attention Is All You Need",
                pair("title", "Attention Is All You Need"),
            ),
        ];
        for (given, expected) in cases {
            assert_eq!(c(given), expected, "{given}");
        }
    }

    #[test]
    fn extended_identifier_forms() {
        assert_eq!(c("PMC10871810"), pair("pmcid", "PMC10871810"));
        assert_eq!(c("pmcid: pmc10871810"), pair("pmcid", "PMC10871810"));
        assert_eq!(
            c("https://pmc.ncbi.nlm.nih.gov/articles/PMC10871810/"),
            pair("pmcid", "PMC10871810")
        );
        assert_eq!(c("PMID: 38270601"), pair("pmid", "38270601"));
        assert_eq!(c("pmid:38270601"), pair("pmid", "38270601"));
        assert_eq!(
            c("https://pubmed.ncbi.nlm.nih.gov/38270601/"),
            pair("pmid", "38270601")
        );
        let s2 = "204e3073870fae3d05bcbc2f6a8e263d9b72e776";
        assert_eq!(c(s2), pair("s2", s2));
        assert_eq!(
            c(
                "https://www.semanticscholar.org/paper/Attention-is-All-you-Need-Vaswani/204e3073870fae3d05bcbc2f6a8e263d9b72e776"
            ),
            pair("s2", s2)
        );
        assert_eq!(c("CorpusId:13756489"), pair("s2", "CorpusId:13756489"));
        // A bare number is a title, not a PMID: "1984" is a book.
        assert_eq!(c("1984"), pair("title", "1984"));
        // DOIs with brackets that belong to them survive; unbalanced ones go.
        assert_eq!(
            c("https://doi.org/10.1016/S0140-6736(20)31180-6"),
            pair("doi", "10.1016/s0140-6736(20)31180-6")
        );
        assert_eq!(
            c("see (doi:10.1016/S0140-6736(20)31180-6)."),
            pair("doi", "10.1016/s0140-6736(20)31180-6")
        );
        assert_eq!(
            c("https://doi.org/10.1000%2Fxyz123"),
            pair("doi", "10.1000/xyz123")
        );
        assert_eq!(
            c("https://dl.acm.org/doi/10.1145/1571941.1572114"),
            pair("doi", "10.1145/1571941.1572114")
        );
    }

    #[test]
    fn work_ids_are_normalised_and_arxiv_dois_move() {
        let mut ids = WorkIds::default();
        ids.doi = Some("https://doi.org/10.48550/arXiv.1706.03762".into());
        ids.pmid = Some("PMID:0038270601".into());
        ids.pmcid = Some("10871810".into());
        ids.openalex = Some("https://openalex.org/W2626778328".into());
        ids.s2 = Some("  ".into());
        normalize_work_ids(&mut ids);
        assert_eq!(ids.doi, None);
        assert_eq!(ids.arxiv.as_deref(), Some("1706.03762"));
        assert_eq!(ids.pmid.as_deref(), Some("38270601"));
        assert_eq!(ids.pmcid.as_deref(), Some("PMC10871810"));
        assert_eq!(ids.openalex.as_deref(), Some("W2626778328"));
        assert_eq!(ids.s2, None);
        assert_eq!(canonical_key(&ids).as_deref(), Some("arxiv:1706.03762"));

        let mut ids = WorkIds::default();
        ids.doi = Some("HTTP://DX.DOI.ORG/10.1145/1571941.1572114".into());
        ids.arxiv = Some("http://arxiv.org/abs/1706.03762v7".into());
        normalize_work_ids(&mut ids);
        assert_eq!(ids.doi.as_deref(), Some("10.1145/1571941.1572114"));
        assert_eq!(ids.arxiv.as_deref(), Some("1706.03762"));
        assert_eq!(
            identity_keys(&ids),
            ["doi:10.1145/1571941.1572114", "arxiv:1706.03762"]
        );
    }

    #[test]
    fn preprint_dois_are_recognised() {
        assert!(is_preprint_doi("10.1101/2020.03.22.002386"));
        assert!(is_preprint_doi("10.48550/arxiv.1706.03762"));
        assert!(is_preprint_doi("10.31234/osf.io/abcd"));
        // Cold Spring Harbor journals share the 10.1101 prefix.
        assert!(!is_preprint_doi("10.1101/gr.123456.111"));
        assert!(!is_preprint_doi("10.1145/1571941.1572114"));
        assert_eq!(arxiv_doi("1706.03762"), "10.48550/arxiv.1706.03762");
        assert_eq!(
            Pid::Arxiv("1706.03762".into()).canonical_key().as_deref(),
            Some("arxiv:1706.03762")
        );
        assert_eq!(Pid::Title("x".into()).canonical_key(), None);
    }

    #[test]
    fn m2_url_query_fragment_case_insensitive_openalex_s2_pmid_pmcid_and_openrxiv() {
        // DOI inside http(s) URLs strips ?query, #fragment, and trailing /.
        let doi = "10.1145/1571941.1572114";
        assert_eq!(
            c("https://doi.org/10.1145/1571941.1572114?utm_source=x"),
            pair("doi", doi)
        );
        assert_eq!(
            c("https://doi.org/10.1145/1571941.1572114#abstract"),
            pair("doi", doi)
        );
        assert_eq!(
            c("https://dl.acm.org/doi/10.1145/1571941.1572114/"),
            pair("doi", doi)
        );
        assert_eq!(
            c("https://dl.acm.org/doi/10.1145/1571941.1572114/?utm_source=x#top"),
            pair("doi", doi)
        );
        // Lowercase OpenAlex ids (bare and URL).
        assert_eq!(c("w2148972377"), pair("openalex", "W2148972377"));
        assert_eq!(
            c("https://openalex.org/w2148972377"),
            pair("openalex", "W2148972377")
        );
        // Uppercase S2 hex lowercased; PMID/PMCID never concatenate unrelated digits.
        let mut ids = WorkIds::default();
        ids.s2 = Some("204E3073870FAE3D05BCBC2F6A8E263D9B72E776".into());
        ids.pmid = Some("https://pubmed.ncbi.nlm.nih.gov/32450107/?v=2".into());
        ids.pmcid = Some("PMC7255293.1".into());
        ids.openalex = Some("https://openalex.org/w2626778328/?foo=1".into());
        normalize_work_ids(&mut ids);
        assert_eq!(
            ids.s2.as_deref(),
            Some("204e3073870fae3d05bcbc2f6a8e263d9b72e776")
        );
        assert_eq!(ids.pmid.as_deref(), Some("32450107"));
        assert_eq!(ids.pmcid.as_deref(), Some("PMC7255293"));
        assert_eq!(ids.openalex.as_deref(), Some("W2626778328"));
        // openRxiv 10.64898/ bioRxiv/medRxiv prefix.
        assert!(is_preprint_doi("10.64898/2026.08.12.744382"));
    }
}
