//! Host tables, source classification and the kernel's own post-filters
//! (ports of `engines/base.py`: `classify_source`, the host allowlists and
//! `apply_post_filters_with_diagnostics`).
//!
//! Engines under-honour URL operators and freshness parameters, so the kernel
//! re-checks every result: domain include/exclude, the scope's host allowlist
//! for general web engines (`news`, `forum`, and `academic` when it falls
//! back to the web pool), and freshness — dropping only on a date it trusts.

use fsm_core::dates::{extract_date_hint, normalize_date};
use fsm_core::url_canon::{host_matches, host_of};
use fsm_core::{Freshness, Hit, HitKind};
use jiff::civil::Date;
use regex::Regex;
use std::sync::LazyLock;

/// Scholarly hosts (`_PAPER_HOSTS`).
pub const PAPER_HOSTS: &[&str] = &[
    "arxiv.org",
    "acm.org",
    "springer.com",
    "ieee.org",
    "nature.com",
    "sciencedirect.com",
    "biorxiv.org",
    "medrxiv.org",
    "openreview.net",
    "paperswithcode.com",
    "semanticscholar.org",
    "plos.org",
    "ssrn.com",
    "jstor.org",
    "mdpi.com",
    "sciencemag.org",
    "frontiersin.org",
    "wiley.com",
    "tandfonline.com",
    "doi.org",
    "ncbi.nlm.nih.gov",
    "europepmc.org",
    "osf.io",
    "zenodo.org",
    "dblp.org",
    "zbmath.org",
    "researchgate.net",
    "aclanthology.org",
    "mlr.press",
    "neurips.cc",
    "openaccess.thecvf.com",
    "cnki.net",
    "ci.nii.ac.jp",
    "j-stage.jst.go.jp",
    "hal.science",
    "scielo.org",
    "cambridge.org",
    "oup.com",
    "sagepub.com",
];

/// Non-scholarly subdomains of scholarly publishers, checked first.
pub const NOT_PAPER_HOSTS: &[&str] = &[
    "dictionary.cambridge.org",
    "shop.cambridge.org",
    "global.oup.com",
    "academic.oup.com/pages",
    "us.sagepub.com",
    "uk.sagepub.com",
];

/// Discussion and Q&A hosts (`_FORUM_HOSTS`).
pub const FORUM_HOSTS: &[&str] = &[
    "reddit.com",
    "news.ycombinator.com",
    "stackoverflow.com",
    "serverfault.com",
    "superuser.com",
    "stackexchange.com",
    "lobste.rs",
    "tildes.net",
    "lemmy.world",
    "lemmy.ml",
    "discourse.org",
    "zhihu.com",
    "v2ex.com",
    "segmentfault.com",
    "juejin.cn",
    "teratail.com",
    "qiita.com",
];

/// Public code forges (`_GITHUB_HOSTS`).
pub const CODE_HOSTS: &[&str] = &[
    "github.com",
    "gist.github.com",
    "gitlab.com",
    "codeberg.org",
    "bitbucket.org",
    "sourceforge.net",
    "savannah.gnu.org",
    "git.sr.ht",
];

/// Known news outlets (`_NEWS_HOSTS`): a floor, not a definition.
pub const NEWS_HOSTS: &[&str] = &[
    "reuters.com",
    "apnews.com",
    "afp.com",
    "bbc.com",
    "bbc.co.uk",
    "nytimes.com",
    "washingtonpost.com",
    "theguardian.com",
    "cnn.com",
    "nbcnews.com",
    "abcnews.go.com",
    "cbsnews.com",
    "foxnews.com",
    "npr.org",
    "bloomberg.com",
    "ft.com",
    "wsj.com",
    "economist.com",
    "cnbc.com",
    "axios.com",
    "politico.com",
    "thehill.com",
    "aljazeera.com",
    "latimes.com",
    "usatoday.com",
    "nypost.com",
    "newsweek.com",
    "time.com",
    "theatlantic.com",
    "newyorker.com",
    "semafor.com",
    "independent.co.uk",
    "telegraph.co.uk",
    "sky.com",
    "cbc.ca",
    "abc.net.au",
    "smh.com.au",
    "straitstimes.com",
    "channelnewsasia.com",
    "scmp.com",
    "japantimes.co.jp",
    "thehindu.com",
    "timesofindia.indiatimes.com",
    "ndtv.com",
    "techcrunch.com",
    "theverge.com",
    "arstechnica.com",
    "wired.com",
    "venturebeat.com",
    "engadget.com",
    "9to5mac.com",
    "9to5google.com",
    "theinformation.com",
    "businessinsider.com",
    "forbes.com",
    "theregister.com",
    "zdnet.com",
    "cnet.com",
    "techradar.com",
    "tomshardware.com",
    "restofworld.org",
    "thenextweb.com",
    "infoq.com",
    "xinhuanet.com",
    "people.com.cn",
    "chinadaily.com.cn",
    "cctv.com",
    "thepaper.cn",
    "caixin.com",
    "yicai.com",
    "jiemian.com",
    "sina.com.cn",
    "163.com",
    "sohu.com",
    "qq.com",
    "ifeng.com",
    "guancha.cn",
    "36kr.com",
    "jiqizhixin.com",
    "qbitai.com",
    "ithome.com",
    "cnbeta.com.tw",
    "udn.com",
    "ltn.com.tw",
    "chinatimes.com",
    "cna.com.tw",
    "hk01.com",
    "nikkei.com",
    "asahi.com",
    "yomiuri.co.jp",
    "nhk.or.jp",
    "mainichi.jp",
    "chosun.com",
    "joongang.co.kr",
    "hani.co.kr",
    "yna.co.kr",
    "lemonde.fr",
    "lefigaro.fr",
    "liberation.fr",
    "spiegel.de",
    "zeit.de",
    "faz.net",
    "welt.de",
    "sueddeutsche.de",
    "elpais.com",
    "elmundo.es",
    "corriere.it",
    "repubblica.it",
    "folha.uol.com.br",
    "globo.com",
    "tass.com",
    "themoscowtimes.com",
    "haaretz.com",
    "timesofisrael.com",
    "euronews.com",
    "euractiv.com",
    "politico.eu",
    "dw.com",
    "france24.com",
    "theconversation.com",
    "bleepingcomputer.com",
    "therecord.media",
    "darkreading.com",
    "helpnetsecurity.com",
    "propublica.org",
    "statnews.com",
    "404media.co",
    "kyodonews.net",
    "koreaherald.com",
    "koreatimes.co.kr",
    "taipeitimes.com",
    "focustaiwan.tw",
    "natlawreview.com",
    "techpolicy.press",
    "fortune.com",
    "fastcompany.com",
    "technologyreview.com",
    "news.google.com",
];

const NEWS_HOST_PREFIXES: [&str; 2] = ["news.", "newsroom."];
const NEWS_HOST_SUFFIXES: [&str; 5] = [".press", "news.com", "news.net", "news.cn", "news.org"];

/// Allowlist membership or the `news.<domain>` naming convention.
pub fn is_news_host(host: &str) -> bool {
    host_matches(host, NEWS_HOSTS)
        || NEWS_HOST_PREFIXES.iter().any(|p| host.starts_with(p))
        || NEWS_HOST_SUFFIXES.iter().any(|s| host.ends_with(s))
}

/// A scholarly host, and not one of the presses' non-scholarly siblings.
pub fn is_paper_host(host: &str) -> bool {
    !host_matches(host, NOT_PAPER_HOSTS) && host_matches(host, PAPER_HOSTS)
}

fn is_government_host(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    let n = labels.len();
    let cc = |l: &str| l.len() == 2 && l.bytes().all(|b| b.is_ascii_lowercase());
    // (^|\.)(gov|mil)(\.[a-z]{2})?$
    if n >= 1 && matches!(labels[n - 1], "gov" | "mil") {
        return true;
    }
    if n >= 2 && matches!(labels[n - 2], "gov" | "mil") && cc(labels[n - 1]) {
        return true;
    }
    // \.(gouv\.fr|go\.jp|go\.kr|gc\.ca|gob\.[a-z]{2}|europa\.eu)$
    if n >= 3 {
        let (a, b) = (labels[n - 2], labels[n - 1]);
        return matches!(
            (a, b),
            ("gouv", "fr") | ("go", "jp") | ("go", "kr") | ("gc", "ca") | ("europa", "eu")
        ) || (a == "gob" && cc(b));
    }
    false
}

fn is_academic_host(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    let n = labels.len();
    let cc = |l: &str| l.len() == 2 && l.bytes().all(|b| b.is_ascii_lowercase());
    // (^|\.)edu(\.[a-z]{2})?$ | \.ac\.[a-z]{2}$
    (n >= 1 && labels[n - 1] == "edu")
        || (n >= 2 && labels[n - 2] == "edu" && cc(labels[n - 1]))
        || (n >= 3 && labels[n - 2] == "ac" && cc(labels[n - 1]))
}

/// What KIND of site a result comes from, from its hostname alone:
/// `paper`, `code`, `forum`, `news`, `government`, `academic` or `""`.
/// A description for the reader, never a ranking signal.
pub fn classify_source(url: &str) -> &'static str {
    let host = host_of(url);
    if host.is_empty() {
        ""
    } else if is_paper_host(&host) {
        "paper"
    } else if host_matches(&host, CODE_HOSTS) {
        "code"
    } else if host_matches(&host, FORUM_HOSTS) {
        "forum"
    } else if is_news_host(&host) {
        "news"
    } else if is_government_host(&host) {
        "government"
    } else if is_academic_host(&host) {
        "academic"
    } else {
        ""
    }
}

/// The host allowlist a scope imposes on GENERAL web engines' results
/// (sources native to the scope are never host-filtered).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostClass {
    News,
    Forum,
    Paper,
}

impl HostClass {
    /// The allowlist for a canonical scope id, if it has one.
    pub fn for_scope(scope: &str) -> Option<Self> {
        match fsm_core::scopes::root(scope) {
            "news" => Some(Self::News),
            "forum" => Some(Self::Forum),
            "academic" => Some(Self::Paper),
            _ => None,
        }
    }

    fn admits(self, host: &str) -> bool {
        match self {
            Self::News => is_news_host(host),
            Self::Forum => host_matches(host, FORUM_HOSTS),
            Self::Paper => is_paper_host(host),
        }
    }

    /// The drop-reason label (`scope=news`, ...).
    pub fn label(self) -> &'static str {
        match self {
            Self::News => "scope=news",
            Self::Forum => "scope=forum",
            Self::Paper => "scope=academic",
        }
    }
}

/// The caller's filters, parsed once.
#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub freshness: Option<Freshness>,
}

impl Filters {
    /// `domains` entries are `"x.com"` (include) or `"-x.com"` (exclude).
    pub fn new(domains: &[String], freshness: Option<Freshness>) -> Self {
        let mut f = Self {
            freshness,
            ..Self::default()
        };
        for d in domains {
            let d = d.trim();
            let (list, raw) = match d.strip_prefix('-') {
                Some(rest) => (&mut f.exclude, rest),
                None => (&mut f.include, d),
            };
            let norm = raw.trim().trim_start_matches('.').to_ascii_lowercase();
            if !norm.is_empty() && !list.contains(&norm) {
                list.push(norm);
            }
        }
        f
    }
}

/// Why results were dropped, by reason, in first-seen order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Drops(pub Vec<(&'static str, usize)>);

impl Drops {
    pub fn bump(&mut self, reason: &'static str) {
        match self.0.iter_mut().find(|(r, _)| *r == reason) {
            Some((_, n)) => *n += 1,
            None => self.0.push((reason, 1)),
        }
    }

    pub fn total(&self) -> usize {
        self.0.iter().map(|(_, n)| n).sum()
    }

    pub fn merge(&mut self, other: &Drops) {
        for (r, n) in &other.0 {
            for _ in 0..*n {
                self.bump(r);
            }
        }
    }

    /// The reason that dropped the most (first on ties).
    pub fn top(&self) -> Option<(&'static str, usize)> {
        self.0.iter().copied().fold(None, |best, cur| match best {
            Some(b) if b.1 >= cur.1 => Some(b),
            _ => Some(cur),
        })
    }
}

/// Window of each freshness value, in days (generous: month 31, year 366).
pub fn freshness_max_days(f: Freshness) -> f64 {
    match f {
        Freshness::Day => 1.0,
        Freshness::Week => 7.0,
        Freshness::Month => 31.0,
        _ => 366.0,
    }
}

static REL_AGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b([0-9]+)\s*(minute|hour|day|week|month|year)s?\s*ago\b").expect("regex")
});

fn relative_age_en(s: &str) -> Option<f64> {
    let c = REL_AGE_RE.captures(s)?;
    let n: f64 = c[1].parse().ok()?;
    let per = match c[2].to_ascii_lowercase().as_str() {
        "minute" => 1.0 / 1440.0,
        "hour" => 1.0 / 24.0,
        "day" => 1.0,
        "week" => 7.0,
        "month" => 30.0,
        "year" => 365.0,
        _ => return None,
    };
    Some(n * per)
}

/// `N unit(s) ago` (EN or CJK relative phrase) → age in days.
fn relative_age(s: &str) -> Option<f64> {
    if let Some(days) = relative_age_en(s) {
        return Some(days);
    }
    // Accept unnormalised CJK relative phrases ("10天前", "3일 전", "2週間前").
    let norm = extract_date_hint(s, Date::constant(2000, 1, 1))?;
    relative_age_en(&norm)
}

/// The first `YYYY-MM-DD` in `s`.
fn iso_date(s: &str) -> Option<Date> {
    let b = s.as_bytes();
    (0..b.len().saturating_sub(9)).find_map(|i| {
        let w = &b[i..i + 10];
        let shape = w.iter().enumerate().all(|(j, c)| {
            if j == 4 || j == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        });
        // Digits must not continue on either side; a following `T08:00` is fine
        // (structured timestamps), unlike Python's `\b`.
        let bounded = (i == 0 || !b[i - 1].is_ascii_digit())
            && b.get(i + 10).is_none_or(|c| !c.is_ascii_digit());
        if !(shape && bounded) {
            return None;
        }
        let y = s[i..i + 4].parse().ok()?;
        let m = s[i + 5..i + 7].parse().ok()?;
        let d = s[i + 8..i + 10].parse().ok()?;
        Date::new(y, m, d).ok()
    })
}

/// Fallback for non-ISO structured dates (RFC 2822, month names, CJK, or
/// partial `YYYY-MM` / `YYYY` dates). Partial dates resolve conservatively to
/// the last day of that month/year so freshness never over-drops a record.
fn normalized_conservative_date(s: &str) -> Option<Date> {
    let norm = normalize_date(s)?;
    let parts: Vec<&str> = norm.split('-').collect();
    match parts.as_slice() {
        [y, m, d] => Date::new(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?).ok(),
        [y, m] => {
            let first = Date::new(y.parse().ok()?, m.parse().ok()?, 1).ok()?;
            Some(first.last_of_month())
        }
        [y] => Date::new(y.parse().ok()?, 12, 31).ok(),
        _ => None,
    }
}

/// Age of a date hint in days (`_published_age_in_days`): `"N units ago"`
/// (EN/CJK), a date containing `YYYY-MM-DD`, or a structured date string
/// recognised by `normalize_date`; `None` = unknown (keep).
pub fn age_in_days(hint: &str, today: Date) -> Option<f64> {
    let s = hint.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(days) = relative_age(s) {
        return Some(days);
    }
    let d = iso_date(s).or_else(|| normalized_conservative_date(s))?;
    let span = today.since(d).ok()?;
    Some(f64::from(span.get_days()).max(0.0))
}

/// Whether `h`'s date may DROP it: a relative "N ago" phrase or a date from a
/// structured source. A date scraped from snippet prose is display-only.
pub fn trusted_date(h: &Hit) -> bool {
    crate::rank::is_confident(h) || h.date.as_deref().is_some_and(|d| relative_age(d).is_some())
}

/// `apply_post_filters_with_diagnostics`: each dropped result counts against
/// the first filter that rejected it.
pub fn apply(
    hits: Vec<Hit>,
    f: &Filters,
    host_class: Option<HostClass>,
    today: Date,
) -> (Vec<Hit>, Drops) {
    let mut drops = Drops::default();
    if f.include.is_empty() && f.exclude.is_empty() && f.freshness.is_none() && host_class.is_none()
    {
        return (hits, drops);
    }
    let inc: Vec<&str> = f.include.iter().map(String::as_str).collect();
    let exc: Vec<&str> = f.exclude.iter().map(String::as_str).collect();
    let mut out = Vec::with_capacity(hits.len());
    for h in hits {
        let host = host_of(&h.url);
        if !inc.is_empty() && !host_matches(&host, &inc) {
            drops.bump("include_domains");
            continue;
        }
        if !exc.is_empty() && host_matches(&host, &exc) {
            drops.bump("exclude_domains");
            continue;
        }
        if let Some(class) = host_class
            && !(class == HostClass::News && h.kind == HitKind::News)
            && !class.admits(&host)
        {
            drops.bump(class.label());
            continue;
        }
        if let Some(fr) = f.freshness
            && let Some(age) = h.date.as_deref().and_then(|d| age_in_days(d, today))
            && trusted_date(&h)
            && age > freshness_max_days(fr)
        {
            drops.bump("freshness");
            continue;
        }
        out.push(h);
    }
    (out, drops)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn today() -> Date {
        Date::new(2026, 11, 25).unwrap()
    }

    fn hit(url: &str) -> Hit {
        Hit::new("t", url)
    }

    fn dated(url: &str, date: &str, src: Option<&str>) -> Hit {
        let mut h = hit(url);
        h.date = Some(date.to_owned());
        h.date_source = src.map(str::to_owned);
        h
    }

    fn urls(v: &[Hit]) -> Vec<&str> {
        v.iter().map(|h| h.url.as_str()).collect()
    }

    // --- tests/test_filters.py (verbatim; Python's `category=` is the scope allowlist) ---

    #[test]
    fn post_filter_include_domains() {
        let f = Filters::new(&["github.com".into()], None);
        let (kept, _) = apply(
            vec![hit("https://github.com/a"), hit("https://example.com/b")],
            &f,
            None,
            today(),
        );
        assert_eq!(urls(&kept), ["https://github.com/a"]);
    }

    #[test]
    fn post_filter_include_domains_subdomain() {
        let f = Filters::new(&["github.com".into()], None);
        let (kept, _) = apply(
            vec![hit("https://gist.github.com/x"), hit("https://other.com/y")],
            &f,
            None,
            today(),
        );
        assert_eq!(urls(&kept), ["https://gist.github.com/x"]);
    }

    #[test]
    fn post_filter_exclude_domains() {
        let f = Filters::new(&["-github.com".into()], None);
        let (kept, _) = apply(
            vec![hit("https://github.com/a"), hit("https://example.com/b")],
            &f,
            None,
            today(),
        );
        assert_eq!(urls(&kept), ["https://example.com/b"]);
    }

    #[test]
    fn post_filter_category_paper() {
        let (kept, _) = apply(
            vec![
                hit("https://arxiv.org/abs/1234"),
                hit("https://www.nature.com/article"),
                hit("https://example.com/x"),
            ],
            &Filters::default(),
            Some(HostClass::Paper),
            today(),
        );
        assert_eq!(
            urls(&kept),
            [
                "https://arxiv.org/abs/1234",
                "https://www.nature.com/article"
            ]
        );
    }

    #[test]
    fn post_filter_category_forum() {
        let (kept, _) = apply(
            vec![
                hit("https://www.reddit.com/r/x"),
                hit("https://news.ycombinator.com/item?id=1"),
                hit("https://example.com/x"),
            ],
            &Filters::default(),
            Some(HostClass::Forum),
            today(),
        );
        assert_eq!(kept.len(), 2);
        assert!(
            kept.iter()
                .all(|h| h.url.contains("reddit") || h.url.contains("ycombinator"))
        );
    }

    #[test]
    fn post_filter_empty_returns_input_unchanged() {
        let input = vec![hit("https://a.com"), hit("https://b.com")];
        let (kept, drops) = apply(input.clone(), &Filters::default(), None, today());
        assert_eq!(kept, input);
        assert!(drops.0.is_empty());
    }

    #[test]
    fn publisher_subdomains_that_are_not_scholarly() {
        let cases = [
            ("dictionary.cambridge.org", false),
            ("global.oup.com", false),
            ("us.sagepub.com", false),
            ("www.cambridge.org", true),
            ("academic.oup.com", true),
            ("journals.sagepub.com", true),
            ("arxiv.org", true),
            ("doi.org", true),
            ("example.com", false),
        ];
        for (host, scholarly) in cases {
            assert_eq!(is_paper_host(host), scholarly, "{host}");
        }
    }

    #[test]
    fn a_dictionary_entry_is_dropped_from_a_paper_search() {
        let (kept, drops) = apply(
            vec![
                Hit::new(
                    "RECIPROCAL | English meaning - Cambridge Dictionary",
                    "https://dictionary.cambridge.org/dictionary/english/reciprocal",
                ),
                Hit::new(
                    "Reciprocal rank fusion",
                    "https://doi.org/10.1145/1571941.1572114",
                ),
            ],
            &Filters::default(),
            Some(HostClass::Paper),
            today(),
        );
        assert_eq!(urls(&kept), ["https://doi.org/10.1145/1571941.1572114"]);
        assert_eq!(drops.0, [("scope=academic", 1)]);
    }

    // --- tests/test_category_native_and_locale.py (host allowlists) ---

    #[test]
    fn non_native_category_still_filters_by_host() {
        let (kept, _) = apply(
            vec![hit("https://github.com/a/b")],
            &Filters::default(),
            Some(HostClass::News),
            today(),
        );
        assert!(kept.is_empty());

        // Hits explicitly typed as HitKind::News (e.g. You.com news section) pass
        // HostClass::News even when the outlet is not in the static allowlist.
        let mut news_hit = hit("https://regional-policy-outlet.example/eu-ai-act");
        news_hit.kind = HitKind::News;
        let (kept_news, _) = apply(
            vec![news_hit],
            &Filters::default(),
            Some(HostClass::News),
            today(),
        );
        assert_eq!(kept_news.len(), 1);
        assert!(is_news_host("www.euractiv.com"));
        assert!(is_news_host("www.euronews.com"));
        assert!(is_news_host("www.politico.eu"));
    }

    #[test]
    fn native_category_still_honors_domain_and_freshness_filters() {
        // A native source runs with no host class; the other filters still apply.
        let f = Filters::new(&["arxiv.org".into()], None);
        let (kept, _) = apply(
            vec![
                hit("https://doi.org/10.1/a"),
                hit("https://arxiv.org/abs/1"),
            ],
            &f,
            None,
            today(),
        );
        assert_eq!(urls(&kept), ["https://arxiv.org/abs/1"]);
        let f = Filters::new(&[], Some(Freshness::Day));
        let (kept, _) = apply(
            vec![dated("https://doi.org/10.1/old", "3 years ago", None)],
            &f,
            None,
            today(),
        );
        assert!(kept.is_empty());
    }

    #[test]
    fn news_allowlist_covers_non_anglophone_outlets() {
        for host in [
            "www.thepaper.cn",
            "news.sina.com.cn",
            "36kr.com",
            "www.scmp.com",
            "www.nikkei.com",
            "www.lemonde.fr",
            "www.spiegel.de",
            "www.reuters.com",
        ] {
            assert!(is_news_host(host), "{host}");
        }
    }

    #[test]
    fn news_host_convention_matches_unlisted_outlets() {
        assert!(is_news_host("news.daheiai.com"));
        assert!(is_news_host("newsroom.example.co.jp"));
    }

    #[test]
    fn news_convention_does_not_overmatch() {
        for host in [
            "postgresql.org",
            "newsletter.example.com",
            "github.com",
            "arxiv.org",
        ] {
            assert!(!is_news_host(host), "{host}");
        }
    }

    // --- tests/test_filter_diagnostics.py ---

    #[test]
    fn diagnostics_drops_counted_per_reason() {
        let input = vec![
            hit("https://github.com/a"),
            hit("https://example.com/b"),
            hit("https://spam.com/c"),
        ];
        let f = Filters::new(&["github.com".into()], None);
        let (kept, drops) = apply(input, &f, None, today());
        assert_eq!(urls(&kept), ["https://github.com/a"]);
        assert_eq!(drops.0, [("include_domains", 2)]);
        assert_eq!(kept.len() + drops.total(), 3);
    }

    #[test]
    fn diagnostics_drops_attribute_to_first_failing_filter() {
        let f = Filters::new(&["github.com".into()], None);
        let (kept, drops) = apply(
            vec![hit("https://github.com/a"), hit("https://example.com/b")],
            &f,
            Some(HostClass::Paper),
            today(),
        );
        assert!(kept.is_empty());
        let mut got = drops.0.clone();
        got.sort();
        assert_eq!(got, [("include_domains", 1), ("scope=academic", 1)]);
    }

    #[test]
    fn diagnostics_no_filters_returns_empty_drops() {
        let input = vec![hit("https://a.com"), hit("https://b.com")];
        let (kept, drops) = apply(input.clone(), &Filters::default(), None, today());
        assert_eq!(kept, input);
        assert!(drops.0.is_empty());
    }

    // --- 1.0 additions ---

    #[test]
    fn exclude_domains_drops_matching_hosts() {
        let f = Filters::new(&["-pinterest.com".into(), "-.quora.com".into()], None);
        let (kept, drops) = apply(
            vec![
                hit("https://www.pinterest.com/x"),
                hit("https://quora.com/y"),
                hit("https://ok.org/z"),
            ],
            &f,
            None,
            today(),
        );
        assert_eq!(urls(&kept), ["https://ok.org/z"]);
        assert_eq!(drops.total(), 2);
    }

    #[test]
    fn host_match_is_suffix_on_label_boundary() {
        let f = Filters::new(&["hub.com".into()], None);
        let (kept, _) = apply(vec![hit("https://github.com/a")], &f, None, today());
        assert!(kept.is_empty(), "github.com is not a subdomain of hub.com");
    }

    #[test]
    fn each_drop_is_counted_once_against_the_first_filter() {
        let f = Filters::new(
            &["example.com".into(), "-bad.example.com".into()],
            Some(Freshness::Week),
        );
        let (kept, drops) = apply(
            vec![
                hit("https://other.org/1"),
                hit("https://bad.example.com/2"),
                dated("https://example.com/3", "30 days ago", None),
                hit("https://example.com/4"),
            ],
            &f,
            None,
            today(),
        );
        assert_eq!(urls(&kept), ["https://example.com/4"]);
        assert_eq!(
            drops.0,
            [
                ("include_domains", 1),
                ("exclude_domains", 1),
                ("freshness", 1)
            ]
        );
        assert_eq!(drops.total(), 3);
        assert_eq!(drops.top(), Some(("include_domains", 1)));
    }

    // --- freshness: only trusted dates drop (test_filters.py freshness cases) ---

    #[test]
    fn freshness_drops_stale_relative_and_structured_dates_only() {
        let f = Filters::new(&[], Some(Freshness::Week));
        let (kept, drops) = apply(
            vec![
                dated("https://a.com/rel-old", "10 days ago", None),
                dated("https://a.com/rel-new", "2 days ago", None),
                dated("https://a.com/api-old", "2026-01-01", Some("api")),
                dated("https://a.com/scraped-old", "2009-05-01", Some("snippet")),
                dated("https://a.com/unparsable", "sometime", Some("api")),
                hit("https://a.com/undated"),
            ],
            &f,
            None,
            today(),
        );
        assert_eq!(
            urls(&kept),
            [
                "https://a.com/rel-new",
                "https://a.com/scraped-old",
                "https://a.com/unparsable",
                "https://a.com/undated"
            ]
        );
        assert_eq!(drops.0, [("freshness", 2)]);
    }

    #[test]
    fn freshness_windows_are_generous() {
        let f = Filters::new(&[], Some(Freshness::Month));
        let (kept, _) = apply(
            vec![dated("https://a.com", "2026-10-26", Some("rss"))],
            &f,
            None,
            today(),
        );
        assert_eq!(kept.len(), 1, "30 days old is inside month=31");
        let f = Filters::new(&[], Some(Freshness::Day));
        let (kept, _) = apply(
            vec![dated("https://a.com", "5 hours ago", None)],
            &f,
            None,
            today(),
        );
        assert_eq!(kept.len(), 1);
        let (kept, _) = apply(
            vec![dated("https://a.com", "2026-12-01", Some("api"))],
            &f,
            None,
            today(),
        );
        assert_eq!(kept.len(), 1, "future-dated counts as now");
    }

    #[test]
    fn age_in_days_parses_both_hint_shapes() {
        assert_eq!(age_in_days("3 days ago", today()), Some(3.0));
        assert_eq!(age_in_days("2 weeks ago", today()), Some(14.0));
        assert_eq!(age_in_days("1 hour ago", today()), Some(1.0 / 24.0));
        assert_eq!(age_in_days("2026-11-20", today()), Some(5.0));
        assert_eq!(age_in_days("2026-11-20T08:00:00Z", today()), Some(5.0));
        assert_eq!(age_in_days("", today()), None);
        assert_eq!(age_in_days("2026-13-40", today()), None);
        assert_eq!(age_in_days("yesterday", today()), None);
    }

    #[test]
    fn relative_age_handles_punctuation_and_cjk() {
        assert_eq!(age_in_days("3 days ago.", today()), Some(3.0));
        assert_eq!(age_in_days("(2 weeks ago)", today()), Some(14.0));
        assert_eq!(
            age_in_days("Updated 10 days ago, editor", today()),
            Some(10.0)
        );
        assert_eq!(age_in_days("10天前", today()), Some(10.0));
        assert_eq!(age_in_days("3일 전", today()), Some(3.0));
        assert_eq!(age_in_days("2週間前", today()), Some(14.0));
        assert!(trusted_date(&dated(
            "https://a.com/1",
            "10 days ago.",
            None
        )));
        assert!(trusted_date(&dated(
            "https://a.com/2",
            "10天前",
            Some("snippet")
        )));
        assert!(!trusted_date(&dated(
            "https://a.com/3",
            "2020-01-01",
            Some("snippet")
        )));
    }

    #[test]
    fn age_in_days_handles_non_iso_and_partial_structured_dates() {
        // RFC 2822 and CJK structured dates
        assert_eq!(
            age_in_days("Fri, 20 Nov 2026 08:00:00 GMT", today()),
            Some(5.0)
        );
        assert_eq!(age_in_days("2026年11月20日", today()), Some(5.0));
        // Partial dates resolve conservatively to the end of the month/year:
        // 2026-10 -> 2026-10-31 (25 days before 2026-11-25), 2026-11 -> 0 days.
        assert_eq!(age_in_days("2026-10", today()), Some(25.0));
        assert_eq!(age_in_days("2026-11", today()), Some(0.0));
        // 2020 -> 2020-12-31 (stale under Freshness::Year), 2026 -> 0 days (kept).
        assert!(age_in_days("2020", today()).unwrap() > 366.0);
        assert_eq!(age_in_days("2026", today()), Some(0.0));
        let f = Filters::new(&[], Some(Freshness::Year));
        let (kept, drops) = apply(
            vec![
                dated("https://a.com/old", "2020", Some("api")),
                dated("https://a.com/cur", "2026", Some("api")),
            ],
            &f,
            None,
            today(),
        );
        assert_eq!(urls(&kept), ["https://a.com/cur"]);
        assert_eq!(drops.0, [("freshness", 1)]);
    }

    // --- classify_source (tests/test_freshness_signals.py / base.py docs) ---

    #[test]
    fn classify_source_vectors() {
        // tests/test_freshness_signals.py::test_classify_source (parametrized, verbatim)
        let cases = [
            ("https://arxiv.org/abs/1706.03762", "paper"),
            ("https://github.com/python/cpython", "code"),
            ("https://stackoverflow.com/questions/1", "forum"),
            ("https://www.reuters.com/markets/", "news"),
            ("https://news.example.org/story", "news"),
            (
                "https://www.federalreserve.gov/newsevents.htm",
                "government",
            ),
            ("https://www.gov.uk/guidance/x", "government"),
            ("https://www.moe.gov.cn/notice", "government"),
            ("https://www.army.mil/", "government"),
            ("https://www.service-public.gouv.fr/", "government"),
            ("https://ec.europa.eu/info", "government"),
            ("https://cs.stanford.edu/people", "academic"),
            ("https://www.tsinghua.edu.cn/", "academic"),
            ("https://www.ox.ac.uk/", "academic"),
            ("https://www.robomaster.com/zh-CN", ""),
            ("https://governor.example.com/", ""),
            ("https://education.example.com/", ""),
            ("https://mac.com/", ""),
            ("not a url", ""),
            ("", ""),
        ];
        for (url, kind) in cases {
            assert_eq!(classify_source(url), kind, "{url:?}");
        }
    }
}
