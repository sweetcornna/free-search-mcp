//! Single-source `HOST_API` table and TypeScript `.d.ts` generation (`SPEC.md` §2.4, §3.1–§3.2).

use fsm_core::{HostFn, RenderKind};

/// Namespace order used by both the `exec` tool description and [`dts`].
pub(crate) const NAMESPACES: [&str; 7] = [
    "web", "scholar", "doc", "source", "cache", "catalog", "util",
];

/// Every host function exposed to `exec` scripts (`SPEC.md` §2.4 + §3.2).
///
/// This static slice is the single source of truth for:
/// - `fsm-script`'s sandbox prelude bindings,
/// - `fsm-catalog`'s API index,
/// - the `exec` tool description's TypeScript block (`in_description = true`), and
/// - the full [`dts`] declarations.
///
/// M6 `egress` note: `doc.read` and `doc.outline` are marked `egress = true`
/// (fail-closed) because they accept remote `http(s)://` URLs; `HostServices`
/// permits local paths under `DOCUMENT_ROOT` even after local-doc taint.
pub static HOST_API: &[HostFn] = &[
    // ---- web ----
    HostFn::new(
        "web.ask",
        "ask(q: string, o?: {mode?: \"auto\"|\"fast\"|\"agent\"; freshness?: string; lang?: string; domains?: string[]}): Promise<AskResponse>",
        "Quick factual answer: direct registries or top pages with cited passages, or answer agent when configured.",
        "const ans = await web.ask(\"latest rust stable version\");",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "web.search",
        "search(q: string, o?: {scope?: string; sources?: string[]; freshness?: \"day\"|\"week\"|\"month\"|\"year\"; domains?: string[]; lang?: string; limit?: number; maxAgeHours?: number}): Promise<SearchResponse>",
        "Keyless multi-engine search across web, vertical scopes, cache, or catalog.",
        "const res = await web.search(\"rust async\", {scope: \"docs\", limit: 5});",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "web.fetch",
        "fetch(url: string, o?: {render?: \"auto\"|\"http\"|\"browser\"; forceRefresh?: boolean; maxAgeHours?: number; inlineImage?: boolean}): Promise<PageResult>; // <=50k chars",
        "Fetch and extract clean Markdown from a public http(s) URL (<=50k chars).",
        "const page = await web.fetch(\"https://example.com\");",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "web.fetchBatch",
        "fetchBatch(urls: string[], o?: {render?: \"auto\"|\"http\"|\"browser\"; maxConcurrency?: number}): Promise<PageResult[]>; // <=20 URLs",
        "Fetch up to 20 public http(s) URLs concurrently.",
        "const pages = await web.fetchBatch([\"https://a.example\", \"https://b.example\"]);",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "web.request",
        "request(o: {url: string; method?: \"GET\"|\"HEAD\"|\"POST\"; headers?: Record<string,string>; query?: Record<string,string>; body?: any}): Promise<HttpResponse>; // .body text, .json parsed",
        "Raw SSRF-checked HTTP request returning HttpResponse with `.body` text (never `.text`) and parsed `.json` when Content-Type is JSON. Example: const r = await web.request({url}); const d = r.json ?? JSON.parse(r.body);",
        "const r = await web.request({url}); const d = r.json ?? JSON.parse(r.body);",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "web.structured",
        "structured(url: string): Promise<StructuredMeta>",
        "Extract JSON-LD, OpenGraph, Microdata, and meta tags from a public URL.",
        "const meta = await web.structured(\"https://example.com/article\");",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "web.research",
        "research(q: string, o?: {depth?: number; scope?: string; freshness?: string; domains?: string[]; lang?: string}): Promise<ResearchBrief>; // depth 1..8",
        "Search and fetch top pages (depth 1..8, default 3) into one ResearchBrief.",
        "const brief = await web.research(\"sqlite fts5 unicode61\", {depth: 3});",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "web.compare",
        "compare(q: string, urls: string[]): Promise<CompareItem[]>; // 2-5 URLs",
        "Fetch 2-5 URLs and extract query-relevant excerpts side by side.",
        "const rows = await web.compare(\"pricing\", [\"https://a.example\", \"https://b.example\"]);",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "web.download",
        "download(url: string): Promise<DownloadResult>",
        "Download a remote file to the auto-expiring download directory (<=5 per exec).",
        "const file = await web.download(\"https://example.com/report.pdf\");",
    )
    .with_in_description(true)
    .with_egress(true),
    // ---- scholar ----
    HostFn::new(
        "scholar.search",
        "search(q: string, o?: {subscope?: string; sources?: string[]; sort?: \"relevance\"|\"citations\"|\"recent\"; yearFrom?: number; yearTo?: number; oaOnly?: boolean; limit?: number}): Promise<WorkResult>",
        "Federated keyless-first academic search returning merged, deduplicated ScholarlyWork records.",
        "const res = await scholar.search(\"transformer attention\", {subscope: \"cs\", limit: 5});",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "scholar.work",
        "work(idOrTitle: string): Promise<ScholarlyWork | null>",
        "Resolve a DOI, arXiv ID, PMID, PMCID, OpenAlex ID, or paper title to one merged ScholarlyWork.",
        "const w = await scholar.work(\"arXiv:1706.03762\");",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "scholar.graph",
        "graph(idOrTitle: string, o?: {direction?: \"both\"|\"references\"|\"citations\"; limit?: number}): Promise<CitationGraph>; // 1..50",
        "Fetch a paper's reference and citation neighbourhood (limit 1..50) plus retraction/correction notices.",
        "const g = await scholar.graph(\"arXiv:1706.03762\", {direction: \"citations\", limit: 10});",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "scholar.fulltext",
        "fulltext(idOrDoi: string, o?: {preferStructured?: boolean; sections?: string[]; maxChars?: number}): Promise<FulltextResult>",
        "Retrieve lawful open-access full text (structured JATS/BioC/HTML5 before PDF).",
        "const ft = await scholar.fulltext(\"10.48550/arXiv.1706.03762\", {sections: [\"Introduction\"]});",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "scholar.integrity",
        "integrity(doiOrId: string): Promise<IntegrityReport>",
        "Check retraction notices (Retraction Watch + Crossref + OpenAlex), scite tallies, and DOAJ vetting.",
        "const rep = await scholar.integrity(\"10.1016/s0140-6736(20)31180-6\");",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "scholar.cite",
        "cite(idOrWork: string|ScholarlyWork, style?: \"bibtex\"|\"csl\"|\"apa\"|\"ieee\"|\"chicago\"|\"vancouver\"): Promise<string>",
        "Format a citation in BibTeX, CSL-JSON, APA, IEEE, Chicago, or Vancouver style.",
        "const bib = await scholar.cite(\"arXiv:1706.03762\", \"bibtex\");",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "scholar.author",
        "author(qOrOrcid: string): Promise<AuthorProfile>",
        "Look up an academic author profile by name or ORCID.",
        "const a = await scholar.author(\"0000-0002-1825-0097\");",
    )
    .with_in_description(false)
    .with_egress(true),
    HostFn::new(
        "scholar.org",
        "org(qOrRor: string): Promise<OrgProfile>",
        "Look up a research organization profile by name or ROR ID.",
        "const org = await scholar.org(\"MIT\");",
    )
    .with_in_description(false)
    .with_egress(true),
    HostFn::new(
        "scholar.venue",
        "venue(qOrIssn: string): Promise<VenueProfile>",
        "Look up a journal or conference profile, DOAJ vetting, and ICORE rank by name or ISSN.",
        "const v = await scholar.venue(\"Nature\");",
    )
    .with_in_description(false)
    .with_egress(true),
    HostFn::new(
        "scholar.grants",
        "grants(q: string, o?: {sources?: string[]; limit?: number}): Promise<GrantRecord[]>",
        "Search research grants across NIH RePORTER, NSF Awards, CORDIS, and UKRI.",
        "const gs = await scholar.grants(\"quantum error correction\", {limit: 5});",
    )
    .with_in_description(false)
    .with_egress(true),
    // ---- doc ----
    HostFn::new(
        "doc.read",
        "read(source: string, o?: {start?: number; length?: number}): Promise<DocSlice>",
        "Read a paginated text slice from a remote URL or local file under DOCUMENT_ROOT (PDF, DOCX, PPTX, XLSX, EPUB, CSV, archives).",
        "const slice = await doc.read(\"https://example.com/paper.pdf\", {start: 0, length: 8000});",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "doc.outline",
        "outline(source: string): Promise<DocOutline>",
        "Extract section headings, page counts, or spreadsheet sheet metadata from a document.",
        "const out = await doc.outline(\"https://example.com/paper.pdf\");",
    )
    .with_in_description(true)
    .with_egress(true),
    // ---- source ----
    HostFn::new(
        "source.call",
        "call(opId: string, params?: Record<string,any>): Promise<SourceOutput>",
        "Invoke a cataloged specialist source operation (`<id>.<op>` or `<id>` for `.search`).",
        "const out = await source.call(\"clinicaltrials.search\", {query: \"asthma\", limit: 5});",
    )
    .with_in_description(true)
    .with_egress(true),
    HostFn::new(
        "source.protocol",
        "protocol(kind: string, baseUrl: string, op: string, params?: Record<string,any>): Promise<ProtocolOutput>",
        "Call one of the 18 generic protocol adapters directly against any public endpoint URL.",
        "const out = await source.protocol(\"mediawiki\", \"https://en.wikipedia.org\", \"search\", {query: \"Rust\"});",
    )
    .with_in_description(true)
    .with_egress(true),
    // ---- cache ----
    HostFn::new(
        "cache.search",
        "search(ftsQuery: string, limit?: number): Promise<CacheHit[]>",
        "Run a zero-network SQLite FTS5 search over locally cached pages.",
        "const hits = await cache.search(\"\\\"exact phrase\\\"\", 10);",
    )
    .with_in_description(true)
    .with_egress(false),
    HostFn::new(
        "cache.getPage",
        "getPage(url: string): Promise<PageResult | null>",
        "Read a previously fetched page from the local cache without hitting the network.",
        "const page = await cache.getPage(\"https://example.com\");",
    )
    .with_in_description(true)
    .with_egress(false),
    HostFn::new(
        "cache.put",
        "put(key: string, value: any, o?: {ttlHours?: number; shared?: boolean}): Promise<void>",
        "Store a JSON value (<=2 MiB) in exec_kv under key matching ^[a-zA-Z0-9._:-]{1,128}$ (not starting with `exec:`).",
        "await cache.put(\"notes.v1\", {items: [1, 2]}, {ttlHours: 24});",
    )
    .with_in_description(true)
    .with_egress(false),
    HostFn::new(
        "cache.get",
        "get(key: string): Promise<any>",
        "Read a stored JSON value or an overflow `exec:<id>` handle from exec_kv.",
        "const full = await cache.get(\"exec:ab12cd34\");",
    )
    .with_in_description(true)
    .with_egress(false),
    // ---- catalog ----
    HostFn::new(
        "catalog.search",
        "search(q: string, o?: {group?: string; capability?: string; keylessOnly?: boolean; limit?: number}): Promise<CatalogHit[]>",
        "Zero-network BM25 + fuzzy search over 300+ sources, 18 protocol adapters, and exec host APIs.",
        "const hits = await catalog.search(\"clinical trials\", {keylessOnly: true, limit: 5});",
    )
    .with_in_description(true)
    .with_egress(false),
    HostFn::new(
        "catalog.describe",
        "describe(id: string): Promise<CatalogDetail>",
        "Inspect full parameters, scopes, capabilities, and examples for a cataloged source, protocol, or host API.",
        "const info = await catalog.describe(\"clinicaltrials\");",
    )
    .with_in_description(true)
    .with_egress(false),
    // ---- util ----
    HostFn::new(
        "util.sleep",
        "sleep(ms: number): Promise<void>",
        "Pause for `ms` milliseconds, bounded by the remaining exec deadline (charges 1 host call).",
        "await util.sleep(250);",
    )
    .with_in_description(true)
    .with_egress(false),
    HostFn::new(
        "util.htmlToMd",
        "htmlToMd(html: string): Promise<string>",
        "Convert an HTML string into clean Markdown locally.",
        "const md = await util.htmlToMd(\"<h1>Title</h1><p>Body</p>\");",
    )
    .with_in_description(true)
    .with_egress(false),
    HostFn::new(
        "util.estimateTokens",
        "estimateTokens(text: string): number",
        "Estimate LLM tokens of text (CJK-aware, exact parity with host budget estimator).",
        "const tok = util.estimateTokens(page.content);",
    )
    .with_in_description(true)
    .with_prelude_sync(true)
    .with_egress(false),
    HostFn::new(
        "util.dedupe",
        "dedupe<T>(items: T[], keyFn?: (x: T) => string): T[]",
        "Deduplicate items preserving first occurrence (defaults to canonical URL key or JSON equality).",
        "const uniq = util.dedupe(hits, (h) => h.url);",
    )
    .with_in_description(false)
    .with_prelude_sync(true)
    .with_egress(false),
    HostFn::new(
        "util.grep",
        "grep(text: string, pattern: string|RegExp, contextLines?: number): string[]",
        "Extract matching lines (with optional context window) from text or a page/response object.",
        "const matches = util.grep(page.content, /CVE-2026-\\d+/i, 2);",
    )
    .with_in_description(false)
    .with_prelude_sync(true)
    .with_egress(false),
    HostFn::new(
        "util.pick",
        "pick<T extends object, K extends keyof T>(obj: T, keys: K[]): Pick<T, K>",
        "Project selected own keys from an object (or array of objects).",
        "const slim = util.pick(work, [\"title\", \"year\", \"url\"]);",
    )
    .with_in_description(false)
    .with_prelude_sync(true)
    .with_egress(false),
];

/// Look up a [`HostFn`] entry in [`HOST_API`] by its exact `<namespace>.<name>` identifier.
pub fn host_fn(name: &str) -> Option<&'static HostFn> {
    HOST_API.iter().find(|f| f.name == name)
}

/// Derive the default [`RenderKind`] for a host function directly from its [`HOST_API`]
/// TypeScript return signature (`Promise<...>`), keeping `HOST_API` as the single source
/// of truth for render dispatch (`SPEC.md` §2.5.2, §3.1).
///
/// Note: `web.fetch` with `{inlineImage: true}` dynamically returns [`RenderKind::InlineImage`]
/// rather than its default [`RenderKind::PageResult`].
pub fn render_kind_for_host_fn(name: &str) -> Option<RenderKind> {
    let sig = host_fn(name)?.sig;
    let ret = extract_return_type(sig)?;
    let inner = ret
        .strip_prefix("Promise<")
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(ret)
        .trim();
    match inner {
        "SearchResponse" => Some(RenderKind::SearchResponse),
        "PageResult" | "PageResult | null" => Some(RenderKind::PageResult),
        "PageResult[]" => Some(RenderKind::PageList),
        "DocSlice" => Some(RenderKind::DocSlice),
        "FulltextResult" => Some(RenderKind::FulltextResult),
        "WorkResult" => Some(RenderKind::WorkResult),
        "ScholarlyWork | null" | "ScholarlyWork[]" => Some(RenderKind::WorkList),
        "ResearchBrief" => Some(RenderKind::ResearchBrief),
        "CitationGraph" => Some(RenderKind::CitationGraph),
        "CatalogHit[]" => Some(RenderKind::CatalogList),
        "InlineImage" => Some(RenderKind::InlineImage),
        "AskResponse" => Some(RenderKind::AskResponse),
        _ => None,
    }
}

fn extract_return_type(sig: &str) -> Option<&str> {
    let no_comment = sig.split("; //").next().unwrap_or(sig).trim();
    let no_semi = no_comment.strip_suffix(';').unwrap_or(no_comment).trim();
    let open = no_semi.find('(')?;
    let rest = &no_semi[open + 1..];
    let mut depth = 0usize;
    let mut close_idx = None;
    for (i, ch) in rest.char_indices() {
        match ch {
            '(' | '{' | '[' | '<' => depth += 1,
            ')' if depth == 0 => {
                close_idx = Some(i);
                break;
            }
            ')' | '}' | ']' | '>' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    let after_parens = rest[close_idx? + 1..].trim();
    after_parens.strip_prefix(':').map(str::trim)
}

/// Core camelCase TypeScript interfaces (`SPEC.md` §3.2, 1:1 with `fsm_core::types`).
pub(crate) const DTS_INTERFACES: &str = "\
interface Hit { title: string; url: string; snippet: string; date?: string; dateSource?: string; source: string; sources: string[]; score: number; kind: \"web\"|\"news\"|\"academic\"|\"record\"|\"vertical\"; extra?: any; }
interface SearchAgentSummary { answer?: string; backend?: string; model?: string; error?: string; elapsedMs: number; }
interface AgentLaneReport { seated: string[]; answered: string[]; pending: string[]; hint?: string; }
interface SearchResponse { query: string; scope: string; results: Hit[]; lead?: string; engines: string[]; cached: boolean; cacheAgeS?: number; elapsedMs: number; notes: string[]; cacheKey?: string; agent?: SearchAgentSummary; agentLane?: AgentLaneReport; }
interface AskPassage { text: string; title: string; url: string; date?: string; source?: string; cacheAgeS?: number; score: number; }
interface AskSource { title: string; url: string; date?: string; source?: string; cacheAgeS?: number; }
interface AskResponse { question: string; mode: string; answer?: string; passages: AskPassage[]; sources: AskSource[]; retrievedAt: string; cacheAgeS?: number; elapsedMs: number; partial: boolean; dateNote?: string; backend?: string; model?: string; steps?: number; agentError?: string; notes: string[]; }
interface PageResult { url: string; title: string; content: string; method: \"http\"|\"browser\"|\"cache\"|\"document\"|\"asset\"; tokensEstimated: number; totalChars: number; truncated: boolean; publishedDate?: string; author?: string; sitename?: string; mediaType?: string; bytes?: number; width?: number; height?: number; sha256?: string; retrievedAt: string; cacheAgeS?: number; }
interface ResearchBrief { query: string; search: SearchResponse; pages: PageResult[]; dateNote?: string; tokensEstimated: number; }
interface InlineImage { url: string; mediaType: string; bytes: number; width?: number; height?: number; sha256: string; }
interface HttpResponse { url: string; status: number; headers: Record<string,string>; body: string; json?: any; truncated: boolean; }
interface StructuredMeta { url: string; jsonLd: any[]; opengraph: Record<string,string>; microdata: any[]; metaFallback: Record<string,string>; hint?: string; }
interface CompareItem { url: string; title?: string; publishedDate?: string; excerpt?: string; truncated?: boolean; error?: string; }
interface DownloadResult { url: string; savedPath: string; mediaType: string; bytes: number; sha256: string; expiresInHours: number; width?: number; height?: number; }
interface DocSlice { source: string; format: string; title?: string; content: string; pages?: number; start: number; returnedChars: number; totalChars: number; truncated: boolean; tokensEstimated: number; }
interface DocOutline { source: string; format: string; title?: string; pages?: number; sections: {title: string; level: number; startChar: number; page?: number}[]; sheets?: {name: string; rows: number; cols: number}[]; }
interface WorkIds { doi?: string; arxiv?: string; pmid?: string; pmcid?: string; openalex?: string; s2?: string; dblp?: string; openaire?: string; }
interface ScholarlyWork { title: string; authors: {name: string; orcid?: string; affiliation?: string}[]; year?: number; publishedDate?: string; venue?: string; workType: string; ids: WorkIds; abstractText?: string; citedBy?: number; citedBySource?: string; influentialCitations?: number; oa?: {status: string; url?: string; pdfUrl?: string; structuredFormat?: \"jats\"|\"bioc\"|\"html5\"; license?: string}; retracted: boolean; retractionReason?: string; versions: {kind: \"preprint\"|\"published\"; doi?: string; arxiv?: string; url: string}[]; url: string; sources: string[]; }
interface SearchMeta { query: string; subscope?: string; sources: string[]; elapsedMs: number; notes: string[]; cached: boolean; }
interface WorkResult { works: ScholarlyWork[]; meta: SearchMeta; }
interface CitationGraph { query: string; resolvedAs: string; paper: ScholarlyWork; references: ScholarlyWork[]; citations: ScholarlyWork[]; notices: {kind: string; date?: string; doi?: string; url?: string}[]; notes: string[]; }
interface FulltextResult { id: string; available: boolean; format?: \"jats\"|\"bioc\"|\"html5\"|\"pdf\"|\"text\"; source?: string; license?: string; url?: string; sections?: {heading: string; text: string}[]; content?: string; why?: string; oaStatus?: string; landingUrl?: string; notes?: string[]; }
interface IntegrityReport { doi?: string; isRetracted: boolean; retractionNotices: {nature: string; date?: string; reason?: string; noticeDoi?: string; source: string}[]; scite?: {supporting: number; contradicting: number; mentioning: number}; doajVetted?: boolean; openalexFwci?: number; }
interface AuthorProfile { name: string; orcid?: string; openalexId?: string; affiliations: string[]; worksCount?: number; citedByCount?: number; hIndex?: number; topWorks: ScholarlyWork[]; sources: string[]; }
interface OrgProfile { name: string; ror?: string; openalexId?: string; country?: string; orgType?: string; homepage?: string; worksCount?: number; citedByCount?: number; sources: string[]; }
interface VenueProfile { name: string; issn: string[]; publisher?: string; doajVetted?: boolean; icoreRank?: string; openalexId?: string; worksCount?: number; sources: string[]; }
interface GrantRecord { id: string; title: string; funder: string; pi?: string; org?: string; amount?: number; currency?: string; startDate?: string; endDate?: string; url?: string; source: string; }
interface SourceOutput { source: string; op: string; items: any[]; nextCursor?: string; text?: string; raw?: any; elapsedMs: number; }
interface ProtocolOutput { kind: string; baseUrl: string; op: string; items: any[]; nextCursor?: string; }
interface CacheHit { url: string; title: string; snippet: string; author?: string; date?: string; sitename?: string; }
interface CatalogHit { id: string; kind: \"source\"|\"protocol\"|\"api\"; summary: string; signature: string; example: string; auth: string; tier: number; health: string; }
interface CatalogDetail extends CatalogHit { params: {name: string; ty: string; required: boolean; default?: any; min?: number; max?: number; doc: string}[]; scopes: string[]; caps: string[]; homeUrl?: string; }";

/// Render TypeScript declarations from [`HOST_API`].
///
/// The `exec` description uses object method signatures for the selected host APIs.
/// The full declarations use namespaces and include every host API.
pub(crate) fn render_host_declarations(in_description_only: bool) -> String {
    let (prefix, opening, method_prefix, closing) = if in_description_only {
        ("declare const ", ": {\n", "  ", "};\n")
    } else {
        ("declare namespace ", " {\n", "  function ", "}\n")
    };
    let mut out = String::new();
    for ns in NAMESPACES {
        let funcs: Vec<&HostFn> = HOST_API
            .iter()
            .filter(|f| f.namespace() == ns && (!in_description_only || f.in_description))
            .collect();
        if funcs.is_empty() {
            continue;
        }
        out.push_str(prefix);
        out.push_str(ns);
        out.push_str(opening);
        for f in funcs {
            out.push_str(method_prefix);
            out.push_str(f.sig);
            if !f.sig.contains("; //") && !f.sig.ends_with(';') {
                out.push(';');
            }
            out.push('\n');
        }
        out.push_str(closing);
    }
    out
}

/// Full TypeScript declarations generated from [`HOST_API`] plus all core camelCase
/// interfaces of `SPEC.md` §3.2.
pub fn dts() -> String {
    let mut out = render_host_declarations(false);
    out.push('\n');
    out.push_str(DTS_INTERFACES);
    out.push('\n');
    out
}
