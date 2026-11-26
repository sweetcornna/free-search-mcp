//! In-memory BM25 + ID prefix + EN/ZH synonym + `rapidfuzz` catalog index.

use crate::synonyms::SynonymMap;
use crate::tokenizer::CatalogTokenizer;
use bm25::{Document, SearchEngine, SearchEngineBuilder};
use fsm_core::{
    AuthMode, CatalogDetail, CatalogHit, CatalogParamDoc, CatalogQuery, FsmError, HealthRecord,
    HostFn, OpAuthOverride, OpDecl, Param, ProtocolAdapter, Registry, SourceDecl, scopes,
};
use rapidfuzz::fuzz;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    Source,
    Protocol,
    Api,
}

impl EntryKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Protocol => "protocol",
            Self::Api => "api",
        }
    }

    fn sort_priority(self) -> u8 {
        match self {
            Self::Source => 0,
            Self::Protocol => 1,
            Self::Api => 2,
        }
    }
}

pub(crate) struct CatalogEntry {
    pub(crate) id: String,
    pub(crate) kind: EntryKind,
    pub(crate) name: String,
    pub(crate) aliases: Vec<String>,
    pub(crate) summary: String,
    pub(crate) signature: String,
    pub(crate) example: String,
    pub(crate) auth: String,
    pub(crate) is_keyless: bool,
    pub(crate) tier: u8,
    pub(crate) scopes: Vec<String>,
    pub(crate) caps: Vec<String>,
    pub(crate) op_names: Vec<String>,
    pub(crate) params: Vec<CatalogParamDoc>,
    pub(crate) home_url: Option<String>,
    /// Lowercased searchable identity strings (`id`, `name`, `aliases`) for prefix & fuzzy matching.
    id_lower: String,
    fuzzy_keys: Vec<String>,
    /// Pre-tokenized bag of stemmed/bigram tokens for fast multi-concept coverage scoring.
    doc_tokens: HashSet<String>,
}

pub(crate) struct CatalogIndex {
    entries: Vec<CatalogEntry>,
    bm25: Option<SearchEngine<usize, u32, CatalogTokenizer>>,
    tokenizer: CatalogTokenizer,
    synonyms: SynonymMap,
    /// Document frequency of each normalized token across all entries.
    doc_freq: HashMap<String, usize>,
}

impl CatalogIndex {
    pub(crate) fn build(reg: &Registry, host_api: &'static [HostFn]) -> Self {
        let tokenizer = CatalogTokenizer::new();
        let synonyms = SynonymMap::build(&tokenizer);
        let mut entries = Vec::new();
        let mut docs = Vec::new();

        for src in reg.all() {
            let decl = src.decl();
            let (entry, doc_text) = Self::entry_from_source(decl, &tokenizer);
            let idx = entries.len();
            docs.push(Document::new(idx, doc_text));
            entries.push(entry);
        }

        for proto in reg.protocols() {
            let (entry, doc_text) = Self::entry_from_protocol(proto.as_ref(), &tokenizer);
            let idx = entries.len();
            docs.push(Document::new(idx, doc_text));
            entries.push(entry);
        }

        for host_fn in host_api {
            let (entry, doc_text) = Self::entry_from_host_fn(host_fn, &tokenizer);
            let idx = entries.len();
            docs.push(Document::new(idx, doc_text));
            entries.push(entry);
        }

        let mut doc_freq: HashMap<String, usize> = HashMap::new();
        for entry in &entries {
            for tok in &entry.doc_tokens {
                *doc_freq.entry(tok.clone()).or_insert(0) += 1;
            }
        }

        let bm25 = if docs.is_empty() {
            None
        } else {
            Some(
                SearchEngineBuilder::<usize, u32, CatalogTokenizer>::with_tokenizer_and_documents(
                    CatalogTokenizer::new(),
                    docs,
                )
                .k1(1.5)
                .b(0.35)
                .build(),
            )
        };

        Self {
            entries,
            bm25,
            tokenizer,
            synonyms,
            doc_freq,
        }
    }

    fn entry_from_source(
        decl: &SourceDecl,
        tokenizer: &CatalogTokenizer,
    ) -> (CatalogEntry, String) {
        let primary_op = decl.op("search").or_else(|| decl.ops.first());
        let signature = format_source_signature(decl, primary_op);
        let example = format_source_example(decl, primary_op);
        let (auth, is_keyless) = format_source_auth(decl);
        let caps: Vec<String> = decl.caps.iter().map(|c| c.as_str().to_owned()).collect();
        let op_names: Vec<String> = decl.ops.iter().map(|o| o.name.clone()).collect();
        let params: Vec<CatalogParamDoc> = primary_op
            .map(|o| o.params.iter().map(param_to_doc).collect())
            .unwrap_or_default();

        let id_spaced = decl.id.replace('_', " ");
        let mut text_parts = Vec::with_capacity(16);
        // Field weighting via controlled repetition: id (3x), name (2x), aliases (2x), scopes/caps (2x)
        for _ in 0..3 {
            text_parts.push(decl.id.clone());
            text_parts.push(id_spaced.clone());
        }
        for _ in 0..2 {
            text_parts.push(decl.name.clone());
            for a in &decl.aliases {
                text_parts.push(a.clone());
            }
            for s in &decl.scopes {
                text_parts.push(s.replace('.', " "));
                if let Some(def) = scopes::get(s) {
                    for syn in def.synonyms {
                        text_parts.push((*syn).to_owned());
                    }
                }
            }
            for c in &caps {
                text_parts.push(c.clone());
            }
        }
        text_parts.push(decl.summary.clone());
        for op in &decl.ops {
            text_parts.push(op.name.clone());
            text_parts.push(op.summary.clone());
        }
        let doc_text = text_parts.join(" ");
        let doc_tokens: HashSet<String> = tokenizer
            .analyze_terms(&doc_text)
            .into_iter()
            .map(|(_, stem)| stem)
            .collect();

        let id_lower = decl.id.to_lowercase();
        let mut fuzzy_keys = vec![
            id_lower.clone(),
            id_spaced.to_lowercase(),
            decl.name.to_lowercase(),
        ];
        for a in &decl.aliases {
            fuzzy_keys.push(a.to_lowercase());
        }

        let entry = CatalogEntry {
            id: decl.id.clone(),
            kind: EntryKind::Source,
            name: decl.name.clone(),
            aliases: decl.aliases.clone(),
            summary: decl.summary.clone(),
            signature,
            example,
            auth,
            is_keyless,
            tier: decl.tier,
            scopes: decl.scopes.clone(),
            caps,
            op_names,
            params,
            home_url: decl.home_url.clone(),
            id_lower,
            fuzzy_keys,
            doc_tokens,
        };
        (entry, doc_text)
    }

    fn entry_from_protocol(
        adapter: &dyn ProtocolAdapter,
        tokenizer: &CatalogTokenizer,
    ) -> (CatalogEntry, String) {
        let kind = adapter.kind();
        let ops = adapter.ops();
        let primary_op = ops
            .iter()
            .find(|o| o.name == "search")
            .or_else(|| ops.first());
        let op_names: Vec<String> = ops.iter().map(|o| o.name.clone()).collect();
        let ops_union = if op_names.is_empty() {
            "search".to_owned()
        } else {
            op_names.join("|")
        };
        let params_sig = primary_op.map(format_params_ts).unwrap_or_default();
        let signature = format!(
            "source.protocol({kind:?}, baseUrl: string, op: \"{ops_union}\", params?: {{ {params_sig} }})"
        );
        let example = match primary_op {
            Some(op) => {
                let sample = sample_params_json(op);
                format!(
                    "source.protocol({kind:?}, \"https://example.org\", {:?}, {sample})",
                    op.name
                )
            }
            None => format!("source.protocol({kind:?}, \"https://example.org\", \"search\", {{}})"),
        };

        let summary = match primary_op {
            Some(op) if !op.summary.is_empty() => {
                format!(
                    "Generic {kind} protocol adapter (ops: {}). {}",
                    op_names.join(", "),
                    op.summary
                )
            }
            _ => format!(
                "Generic {kind} protocol adapter (ops: {}).",
                op_names.join(", ")
            ),
        };

        let mut caps = Vec::new();
        for op in ops {
            let cap = match op.name.as_str() {
                "search" | "query" => "search",
                "list" | "list_records" | "list_sets" | "latest" | "products" => "harvest",
                _ => "lookup",
            };
            if !caps.iter().any(|c| c == cap) {
                caps.push(cap.to_owned());
            }
        }
        let params: Vec<CatalogParamDoc> = primary_op
            .map(|o| o.params.iter().map(param_to_doc).collect())
            .unwrap_or_default();

        let kind_spaced = kind.replace('_', " ");
        let mut text_parts = vec![
            kind.to_owned(),
            kind.to_owned(),
            kind.to_owned(),
            kind_spaced.clone(),
            kind_spaced.clone(),
            "protocol adapter generic".to_owned(),
            summary.clone(),
        ];
        for op in ops {
            text_parts.push(op.name.clone());
            text_parts.push(op.summary.clone());
            for p in &op.params {
                text_parts.push(p.name.clone());
                text_parts.push(p.doc.clone());
            }
        }
        let doc_text = text_parts.join(" ");
        let doc_tokens: HashSet<String> = tokenizer
            .analyze_terms(&doc_text)
            .into_iter()
            .map(|(_, stem)| stem)
            .collect();

        let id_lower = kind.to_lowercase();
        let fuzzy_keys = vec![id_lower.clone(), kind_spaced.to_lowercase()];

        let entry = CatalogEntry {
            id: kind.to_owned(),
            kind: EntryKind::Protocol,
            name: format!("{kind} protocol"),
            aliases: vec![kind_spaced],
            summary,
            signature,
            example,
            auth: "keyless".to_owned(),
            is_keyless: true,
            tier: 1,
            scopes: vec!["protocol".to_owned()],
            caps,
            op_names,
            params,
            home_url: None,
            id_lower,
            fuzzy_keys,
            doc_tokens,
        };
        (entry, doc_text)
    }

    fn entry_from_host_fn(
        host_fn: &HostFn,
        tokenizer: &CatalogTokenizer,
    ) -> (CatalogEntry, String) {
        let ns = host_fn.namespace();
        let short_name = host_fn
            .name
            .split_once('.')
            .map_or(host_fn.name, |(_, f)| f);
        let id_spaced = host_fn.name.replace('.', " ");
        let mut caps = vec!["api".to_owned()];
        if host_fn.is_prelude_sync {
            caps.push("prelude".to_owned());
        } else {
            caps.push("exec".to_owned());
        }

        // Omit `host_fn.example` from BM25 indexing so illustrative example arguments
        // (e.g. `catalog.search("clinical trials")`) do not hijack unrelated domain queries.
        // Repeat `host_fn.doc` 2x to balance source entries' 2x `name`/`aliases`/`caps` weighting.
        let doc_text = format!(
            "{0} {0} {0} {1} {1} {2} {2} {3} {3} {4} api host exec",
            host_fn.name, id_spaced, short_name, host_fn.doc, host_fn.sig
        );
        let doc_tokens: HashSet<String> = tokenizer
            .analyze_terms(&doc_text)
            .into_iter()
            .map(|(_, stem)| stem)
            .collect();

        let id_lower = host_fn.name.to_lowercase();
        let fuzzy_keys = vec![
            id_lower.clone(),
            id_spaced.to_lowercase(),
            short_name.to_lowercase(),
        ];

        let entry = CatalogEntry {
            id: host_fn.name.to_owned(),
            kind: EntryKind::Api,
            name: host_fn.name.to_owned(),
            aliases: vec![short_name.to_owned()],
            summary: host_fn.doc.to_owned(),
            signature: host_fn.sig.to_owned(),
            example: host_fn.example.to_owned(),
            auth: "keyless".to_owned(),
            is_keyless: true,
            tier: 1,
            scopes: vec![ns.to_owned()],
            caps,
            op_names: vec![short_name.to_owned()],
            params: parse_sig_params(host_fn.sig),
            home_url: None,
            id_lower,
            fuzzy_keys,
            doc_tokens,
        };
        (entry, doc_text)
    }

    fn matches_filters(entry: &CatalogEntry, q: &CatalogQuery) -> bool {
        if q.keyless_only && !entry.is_keyless {
            return false;
        }
        if let Some(group) = q.group.as_deref().map(str::trim).filter(|s| !s.is_empty())
            && !matches_group(entry, group)
        {
            return false;
        }
        if let Some(cap) = q
            .capability
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            && !matches_capability(entry, cap)
        {
            return false;
        }
        true
    }

    pub(crate) fn search(
        &self,
        q: &CatalogQuery,
        health_map: &HashMap<String, String>,
    ) -> Vec<CatalogHit> {
        let limit = if q.limit == 0 { 10 } else { q.limit as usize }.min(100);
        let query_trimmed = q.query.trim();

        if query_trimmed.is_empty() {
            let mut filtered: Vec<&CatalogEntry> = self
                .entries
                .iter()
                .filter(|e| Self::matches_filters(e, q))
                .collect();
            filtered.sort_by(|a, b| {
                a.kind
                    .sort_priority()
                    .cmp(&b.kind.sort_priority())
                    .then_with(|| a.tier.cmp(&b.tier))
                    .then_with(|| a.id.cmp(&b.id))
            });
            return filtered
                .into_iter()
                .take(limit)
                .map(|e| to_catalog_hit(e, health_map))
                .collect();
        }

        let q_lower = query_trimmed.to_lowercase();
        let analyzed = self.tokenizer.analyze_terms(query_trimmed);
        let orig_stems: Vec<String> = analyzed.iter().map(|(_, s)| s.clone()).collect();

        // Collect per-query-term concept sets (original token + its synonym expansion).
        // Drop spurious CJK boundary-crossing bigrams (e.g. "象数" in "气象数据") that have
        // zero synonyms and zero document frequency so they never dilute multi-concept coverage.
        let mut concept_entries: Vec<(String, Vec<String>, HashSet<String>)> = Vec::new();
        for tok in &orig_stems {
            let syns = self.synonyms.expand(tok);
            let df = self.doc_freq.get(tok).copied().unwrap_or(0);
            let is_cjk_tok = tok.chars().any(crate::tokenizer::is_cjk);
            if is_cjk_tok && syns.is_empty() && df == 0 {
                continue;
            }
            let mut set = HashSet::new();
            set.insert(tok.clone());
            for syn in &syns {
                set.insert(syn.clone());
            }
            concept_entries.push((tok.clone(), syns, set));
        }
        if concept_entries.is_empty() {
            for tok in &orig_stems {
                let mut set = HashSet::new();
                set.insert(tok.clone());
                concept_entries.push((tok.clone(), Vec::new(), set));
            }
        }

        // Compute specificity (concept IDF) for each concept based on how many entries match
        // ANY term in the concept set. Rare domain concepts (e.g. "气象" -> weather, "seismic")
        // outweigh generic meta-concepts / modifier nouns (e.g. "数据", "检索", "events").
        let total_docs = self.entries.len().max(1) as f64;
        let is_generic_modifier = |tok: &str| {
            matches!(
                tok,
                "event"
                    | "data"
                    | "dataset"
                    | "api"
                    | "list"
                    | "info"
                    | "record"
                    | "item"
                    | "page"
                    | "search"
                    | "find"
                    | "lookup"
                    | "queri"
                    | "servic"
                    | "tool"
                    | "数据"
                    | "检索"
                    | "搜索"
                    | "查询"
                    | "信息"
            )
        };
        let has_domain_concept = concept_entries
            .iter()
            .any(|(tok, _, _)| !is_generic_modifier(tok));
        let concept_weights: Vec<f64> = if concept_entries.len() <= 1 {
            vec![1.0; concept_entries.len()]
        } else {
            concept_entries
                .iter()
                .map(|(tok, _, cset)| {
                    let matched_docs = self
                        .entries
                        .iter()
                        .filter(|e| cset.iter().any(|t| e.doc_tokens.contains(t)))
                        .count();
                    let base = if matched_docs == 0 {
                        0.5
                    } else {
                        let ratio = total_docs / (matched_docs as f64);
                        // Dampened log-IDF in [0.15, 1.8]: rare concepts (~5-30 docs) get ~1.2-1.8,
                        // ultra-broad concepts (~250-500 docs) get ~0.15-0.35.
                        (ratio.ln() * 0.45).clamp(0.15, 1.8)
                    };
                    if has_domain_concept && is_generic_modifier(tok) {
                        (base * 0.30).min(0.40)
                    } else {
                        base
                    }
                })
                .collect()
        };

        let mut scores: HashMap<usize, f64> = HashMap::new();

        // 1. BM25 over original query (full weight) + deduplicated per-concept specificity-weighted synonyms.
        // Deduplicate synonym expansions so queries with multiple words from the same synonym group
        // (e.g. "space rocket launches") never triple-count a single shared word (like "space" in "up_space").
        let mut seen_syn_sets: HashSet<Vec<String>> = HashSet::new();
        if let Some(engine) = &self.bm25 {
            for r in engine.search(query_trimmed, self.entries.len()) {
                if Self::matches_filters(&self.entries[r.document.id], q) {
                    *scores.entry(r.document.id).or_insert(0.0) += f64::from(r.score);
                }
            }
            let syn_scale = if concept_entries.len() <= 1 { 3.5 } else { 2.4 };
            for ((_, syns, cset), &weight) in concept_entries.iter().zip(&concept_weights) {
                if syns.is_empty() {
                    continue;
                }
                let mut key: Vec<String> = cset.iter().cloned().collect();
                key.sort();
                if !seen_syn_sets.insert(key) {
                    continue;
                }
                let expanded_query = syns.join(" ");
                let mut concept_hits: Vec<(usize, f64)> = engine
                    .search(&expanded_query, self.entries.len())
                    .into_iter()
                    .filter(|r| Self::matches_filters(&self.entries[r.document.id], q))
                    .map(|r| (r.document.id, f64::from(r.score)))
                    .collect();
                // Normalize per concept so a group with 15 synonyms doesn't drown out a group with 4,
                // keeping synonym weight subordinate to direct BM25 matches on original query terms.
                let max_s = concept_hits.iter().map(|(_, s)| *s).fold(0.0f64, f64::max);
                if max_s > 0.0 {
                    for (doc_id, s) in concept_hits.drain(..) {
                        let norm_s = (s / max_s) * syn_scale * weight;
                        *scores.entry(doc_id).or_insert(0.0) += norm_s;
                    }
                }
            }
        }

        // 2. ID / Name / Alias exact & prefix boost across all filtered entries.
        for (idx, entry) in self.entries.iter().enumerate() {
            if !Self::matches_filters(entry, q) {
                continue;
            }
            let boost = identity_boost(entry, &q_lower, &analyzed);
            if boost > 0.0 {
                *scores.entry(idx).or_insert(0.0) += boost;
            }
        }

        // 3. Multi-concept coverage bonus weighted by concept specificity.
        // Deduplicate identical concept sets so overlapping synonym-group terms do not award
        // fake full-coverage bonuses to entries matching only a single ambiguous word.
        let mut distinct_concepts: Vec<(&str, &HashSet<String>, f64)> = Vec::new();
        let mut seen_cov: HashSet<Vec<String>> = HashSet::new();
        for ((tok, _, cset), &w) in concept_entries.iter().zip(&concept_weights) {
            let mut key: Vec<String> = cset.iter().cloned().collect();
            key.sort();
            if seen_cov.insert(key) {
                distinct_concepts.push((tok.as_str(), cset, w));
            }
        }
        let wants_search = orig_stems.iter().any(|t| {
            matches!(
                t.as_str(),
                "search" | "find" | "discover" | "检索" | "搜索" | "查询"
            )
        });
        if distinct_concepts.len() >= 2 {
            let max_w = distinct_concepts
                .iter()
                .map(|(_, _, w)| *w)
                .fold(0.0f64, f64::max);
            for (&idx, score) in &mut scores {
                let entry = &self.entries[idx];
                let mut covered = 0usize;
                let mut covered_weight = 0.0f64;
                let mut covered_domain = false;
                for &(tok, cset, w) in &distinct_concepts {
                    if cset.iter().any(|t| entry.doc_tokens.contains(t)) {
                        covered += 1;
                        covered_weight += w;
                        if !is_generic_modifier(tok) {
                            covered_domain = true;
                        }
                    }
                }
                if covered == distinct_concepts.len() {
                    *score += 4.5 + covered_weight * 1.2;
                } else if covered > 1 {
                    *score += 1.2 * covered_weight;
                } else if (has_domain_concept && !covered_domain)
                    || (max_w >= 1.0 && covered_weight < max_w * 0.45)
                {
                    // Entry only matched a low-specificity generic modifier (e.g. "data" or "events")
                    // and missed all primary domain concepts (e.g. "气象" or "seismic").
                    *score *= 0.08;
                } else if covered_domain {
                    // Reward entries corroborated by multiple distinct domain vocabulary terms
                    // over single-word homonym hits (e.g. "up_space" matching only "space").
                    let mut domain_terms: HashSet<&str> = HashSet::new();
                    for &(tok, cset, _) in &distinct_concepts {
                        if !is_generic_modifier(tok) {
                            for t in cset {
                                if entry.doc_tokens.contains(t) {
                                    domain_terms.insert(t.as_str());
                                }
                            }
                        }
                    }
                    if domain_terms.len() >= 2 {
                        *score += 1.8 * ((domain_terms.len().min(4) - 1) as f64);
                    }
                }
            }
        } else if orig_stems.len() >= 2 {
            // All query terms map to the same synonym group (e.g. "space rocket launches"):
            // reward entries matching multiple distinct literal query stems so single-word
            // homonym hits (e.g. "up_space" matching only "space") rank below multi-term matches.
            let unique_stems: HashSet<&str> = orig_stems.iter().map(String::as_str).collect();
            if unique_stems.len() >= 2 {
                for (&idx, score) in &mut scores {
                    let entry = &self.entries[idx];
                    let literal_hits = unique_stems
                        .iter()
                        .filter(|&&st| entry.doc_tokens.contains(st))
                        .count();
                    if literal_hits >= 2 {
                        *score += 3.0 * (literal_hits as f64);
                    }
                }
            }
        }

        // 4. Tier & search-capability tie-breaker (prefer tier 1 & searchable indexes on close ties).
        for (&idx, score) in &mut scores {
            let entry = &self.entries[idx];
            let tier_bonus = match entry.tier {
                1 => 0.35,
                2 => 0.10,
                _ => 0.0,
            };
            *score += tier_bonus;
            if wants_search && entry.caps.iter().any(|c| c == "search") {
                *score += 0.45;
            }
            if entry.scopes.iter().any(|s| s == "academic.index")
                && orig_stems
                    .iter()
                    .any(|t| matches!(t.as_str(), "academ" | "paper" | "scholar" | "学术" | "论文"))
            {
                *score += 0.6;
            }
        }

        let mut ranked: Vec<(usize, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then_with(|| self.entries[a.0].tier.cmp(&self.entries[b.0].tier))
                .then_with(|| self.entries[a.0].id.cmp(&self.entries[b.0].id))
        });

        let mut selected: Vec<usize> = ranked.into_iter().take(limit).map(|(i, _)| i).collect();

        // 5. `rapidfuzz` typo-recovery fallback when BM25 + prefix hits < limit.
        if selected.len() < limit {
            let already: HashSet<usize> = selected.iter().copied().collect();
            let fuzzy_hits = self.fuzzy_fallback(&q_lower, q, &already, limit - selected.len());
            selected.extend(fuzzy_hits);
        }

        selected
            .into_iter()
            .map(|idx| to_catalog_hit(&self.entries[idx], health_map))
            .collect()
    }

    fn fuzzy_fallback(
        &self,
        q_lower: &str,
        q: &CatalogQuery,
        exclude: &HashSet<usize>,
        needed: usize,
    ) -> Vec<usize> {
        let q_chars: Vec<char> = q_lower.chars().collect();
        if q_chars.len() < 2 {
            return Vec::new();
        }
        let q_words: Vec<Vec<char>> = q_lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.chars().count() >= 3)
            .map(|w| w.chars().collect())
            .collect();
        let threshold = if q_chars.len() >= 5 { 0.58 } else { 0.65 };

        let mut candidates: Vec<(usize, f64)> = Vec::new();
        for (idx, entry) in self.entries.iter().enumerate() {
            if exclude.contains(&idx) || !Self::matches_filters(entry, q) {
                continue;
            }
            let mut best = 0.0f64;
            for key in &entry.fuzzy_keys {
                let s = fuzz::ratio(q_chars.iter().copied(), key.chars());
                if s > best {
                    best = s;
                }
                // Word-level typo recovery (e.g. "clincal trals" vs "clinical trials")
                if !q_words.is_empty() {
                    let key_words: Vec<&str> = key
                        .split(|c: char| !c.is_alphanumeric())
                        .filter(|w| !w.is_empty())
                        .collect();
                    if !key_words.is_empty() {
                        let sum: f64 = q_words
                            .iter()
                            .map(|qw| {
                                key_words
                                    .iter()
                                    .map(|kw| fuzz::ratio(qw.iter().copied(), kw.chars()))
                                    .fold(0.0f64, f64::max)
                            })
                            .sum();
                        let avg = sum / (q_words.len() as f64);
                        if avg > best {
                            best = avg;
                        }
                    }
                }
            }
            if best >= threshold {
                candidates.push((idx, best));
            }
        }

        candidates.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then_with(|| self.entries[a.0].tier.cmp(&self.entries[b.0].tier))
                .then_with(|| self.entries[a.0].id.cmp(&self.entries[b.0].id))
        });
        candidates
            .into_iter()
            .take(needed)
            .map(|(i, _)| i)
            .collect()
    }

    pub(crate) fn describe(
        &self,
        id: &str,
        reg: &Registry,
        health_map: &HashMap<String, String>,
    ) -> Result<CatalogDetail, FsmError> {
        let trimmed = id.trim();
        if trimmed.is_empty() {
            return Err(FsmError::invalid("catalog.describe id must not be empty"));
        }

        // Explicit protocol disambiguator: "protocol:<kind>"
        if let Some(proto_id) = trimmed.strip_prefix("protocol:")
            && let Some(entry) = self
                .entries
                .iter()
                .find(|e| e.kind == EntryKind::Protocol && e.id == proto_id)
        {
            return Ok(to_catalog_detail(entry, health_map));
        }

        // 1. Exact API match (e.g. "web.fetch", "scholar.author")
        if let Some(entry) = self
            .entries
            .iter()
            .find(|e| e.kind == EntryKind::Api && e.id == trimmed)
        {
            return Ok(to_catalog_detail(entry, health_map));
        }

        // 2. Exact source match (e.g. "clinicaltrials")
        if let Some(entry) = self
            .entries
            .iter()
            .find(|e| e.kind == EntryKind::Source && e.id == trimmed)
        {
            return Ok(to_catalog_detail(entry, health_map));
        }

        // 3. "<source>.<op>" or "<protocol>.<op>"
        if let Some((head, op_name)) = trimmed.split_once('.') {
            if let Some(src) = reg.get(head) {
                let decl = src.decl();
                if let Some(op) = decl.op(op_name) {
                    return Ok(describe_source_op(decl, op, health_map));
                }
            }
            if let Some(proto) = reg.protocol(head)
                && let Some(op) = proto.ops().iter().find(|o| o.name == op_name)
            {
                return Ok(describe_protocol_op(proto.as_ref(), op));
            }
        }

        // 4. Exact protocol match (e.g. "ckan", "socrata")
        if let Some(entry) = self
            .entries
            .iter()
            .find(|e| e.kind == EntryKind::Protocol && e.id == trimmed)
        {
            return Ok(to_catalog_detail(entry, health_map));
        }

        // 5. Case-insensitive fallback on exact ID
        let low = trimmed.to_lowercase();
        if let Some(entry) = self.entries.iter().find(|e| e.id_lower == low) {
            return Ok(to_catalog_detail(entry, health_map));
        }

        // Helpful error with top fuzzy suggestions
        let suggestions = self.fuzzy_fallback(&low, &CatalogQuery::default(), &HashSet::new(), 3);
        let hint = if suggestions.is_empty() {
            "find ids with catalog.search(...)".to_owned()
        } else {
            let names: Vec<&str> = suggestions
                .iter()
                .map(|&i| self.entries[i].id.as_str())
                .collect();
            format!(
                "did you mean: {}? Or run catalog.search(...)",
                names.join(", ")
            )
        };
        Err(FsmError::invalid(format!(
            "unknown catalog id {trimmed:?}; {hint}"
        )))
    }
}

fn identity_boost(entry: &CatalogEntry, q_lower: &str, analyzed: &[(String, String)]) -> f64 {
    let mut boost = 0.0;
    if entry.id_lower == q_lower {
        boost += 14.0;
    } else if entry.id_lower.starts_with(q_lower) {
        boost += 7.0;
    } else if q_lower.len() >= 3 && entry.id_lower.contains(q_lower) {
        boost += 3.0;
    }

    let name_lower = entry.name.to_lowercase();
    if name_lower == q_lower {
        boost += 10.0;
    } else if name_lower.starts_with(q_lower) {
        boost += 4.5;
    }

    for alias in &entry.aliases {
        let a_low = alias.to_lowercase();
        if a_low == q_lower {
            boost += 10.0;
            break;
        } else if a_low.starts_with(q_lower) {
            boost += 4.5;
            break;
        }
    }

    if analyzed.len() >= 2 {
        for (raw, _) in analyzed {
            if raw.len() >= 2 && (entry.id_lower == *raw || entry.id_lower.starts_with(raw)) {
                boost += 1.5;
            }
        }
    }
    boost
}

fn matches_group(entry: &CatalogEntry, group_raw: &str) -> bool {
    let g = group_raw.to_ascii_lowercase();
    match g.as_str() {
        "source" | "sources" => return entry.kind == EntryKind::Source,
        "protocol" | "protocols" => return entry.kind == EntryKind::Protocol,
        "api" | "apis" | "host" | "exec" => return entry.kind == EntryKind::Api,
        _ => {}
    }
    if entry.kind == EntryKind::Api {
        if entry.scopes.iter().any(|s| s.eq_ignore_ascii_case(&g)) {
            return true;
        }
        if let Some(def) = scopes::resolve(&g)
            && def.id == "academic"
            && entry.scopes.iter().any(|s| s == "scholar")
        {
            return true;
        }
        return false;
    }
    if let Some(def) = scopes::resolve(&g) {
        return entry.scopes.iter().any(|s| scopes::is_within(s, def.id));
    }
    entry
        .scopes
        .iter()
        .any(|s| s.eq_ignore_ascii_case(&g) || scopes::is_within(s, &g))
}

fn matches_capability(entry: &CatalogEntry, cap_raw: &str) -> bool {
    let c = normalize_cap(cap_raw);
    entry.caps.iter().any(|ec| ec.eq_ignore_ascii_case(c))
        || entry.op_names.iter().any(|op| op.eq_ignore_ascii_case(c))
}

fn normalize_cap(raw: &str) -> &str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "grants" => "grant",
        "patents" => "patent",
        "datasets" => "dataset",
        "authors" => "author",
        "orgs" | "organizations" => "org",
        "venues" => "venue",
        "books" => "book",
        "theses" => "thesis",
        "citations" => "graph",
        _ => raw.trim(),
    }
}

fn format_source_auth(decl: &SourceDecl) -> (String, bool) {
    let envs = decl.key_envs();
    match &decl.auth {
        AuthMode::None => ("keyless".to_owned(), true),
        AuthMode::OptionalKey { .. } => {
            let label = if envs.is_empty() {
                "optional".to_owned()
            } else {
                format!("optional({})", envs.join(","))
            };
            (label, true)
        }
        AuthMode::RequiredKey { .. } => {
            let label = if envs.is_empty() {
                "key".to_owned()
            } else {
                format!("key({})", envs.join(","))
            };
            let has_keyless_op = decl.ops.iter().any(|o| o.auth == OpAuthOverride::None);
            (label, has_keyless_op)
        }
        _ => ("keyless".to_owned(), true),
    }
}

fn format_params_ts(op: &OpDecl) -> String {
    op.params
        .iter()
        .map(|p| {
            let opt = if p.required { "" } else { "?" };
            format!("{}{}: {}", p.name, opt, p.ty.ts_name())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_source_signature(decl: &SourceDecl, primary: Option<&OpDecl>) -> String {
    match primary {
        Some(op) => {
            let params = format_params_ts(op);
            let call = format!("source.call(\"{}.{}\", {{ {} }})", decl.id, op.name, params);
            if decl.ops.len() > 1 {
                let ops: Vec<&str> = decl.ops.iter().map(|o| o.name.as_str()).collect();
                format!("{call} [ops: {}]", ops.join(", "))
            } else {
                call
            }
        }
        None => format!("source.call(\"{}.search\", {{ query: string }})", decl.id),
    }
}

fn format_source_example(decl: &SourceDecl, primary: Option<&OpDecl>) -> String {
    if let Some(op) = primary {
        if let Some(ex) = op.examples.first()
            && !ex.trim().is_empty()
        {
            return ex.clone();
        }
        if let Some(probe) = &op.probe {
            let params_json = serde_json::to_string(&probe.params).unwrap_or_else(|_| "{}".into());
            return format!("source.call(\"{}.{}\", {params_json})", decl.id, op.name);
        }
        let sample = sample_params_json(op);
        return format!("source.call(\"{}.{}\", {sample})", decl.id, op.name);
    }
    format!(
        "source.call(\"{}.search\", {{\"query\":\"example\"}})",
        decl.id
    )
}

fn sample_params_json(op: &OpDecl) -> String {
    let mut map = serde_json::Map::new();
    for p in &op.params {
        if p.required {
            let val = match p.ty {
                fsm_core::ParamType::String => Value::String("...".to_owned()),
                fsm_core::ParamType::Int => Value::from(1),
                fsm_core::ParamType::Float => Value::from(1.0),
                fsm_core::ParamType::Bool => Value::Bool(true),
                fsm_core::ParamType::StringArray => Value::Array(vec![Value::String("...".into())]),
                fsm_core::ParamType::Json => Value::Object(serde_json::Map::new()),
                _ => Value::String("...".to_owned()),
            };
            map.insert(p.name.clone(), val);
        }
    }
    Value::Object(map).to_string()
}

fn param_to_doc(p: &Param) -> CatalogParamDoc {
    let mut doc = p.doc.clone();
    if let Some(vals) = &p.enum_values
        && !vals.is_empty()
    {
        if !doc.is_empty() {
            doc.push(' ');
        }
        doc.push_str(&format!("[enum: {}]", vals.join("|")));
    }
    let mut out = CatalogParamDoc::new(&p.name, p.ty.ts_name(), doc);
    out.required = p.required;
    out.default = p.default.clone();
    out.min = p.min;
    out.max = p.max;
    out
}

/// Best-effort parameter extractor from a TypeScript function signature like
/// `fetch(url: string, o?: {render?: string}): Promise<PageResult>`.
fn parse_sig_params(sig: &str) -> Vec<CatalogParamDoc> {
    let Some(open) = sig.find('(') else {
        return Vec::new();
    };
    let rest = &sig[open + 1..];
    let mut depth = 0usize;
    let mut end = None;
    for (i, ch) in rest.char_indices() {
        match ch {
            '(' | '{' | '[' | '<' => depth += 1,
            ')' if depth == 0 => {
                end = Some(i);
                break;
            }
            ')' | '}' | ']' | '>' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    let Some(close) = end else {
        return Vec::new();
    };
    let inside = rest[..close].trim();
    if inside.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut d = 0usize;
    let mut segments = Vec::new();
    for (i, ch) in inside.char_indices() {
        match ch {
            '(' | '{' | '[' | '<' => d += 1,
            ')' | '}' | ']' | '>' => d = d.saturating_sub(1),
            ',' if d == 0 => {
                segments.push(inside[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    segments.push(inside[start..].trim());

    for seg in segments {
        if let Some((raw_name, raw_ty)) = seg.split_once(':') {
            let trimmed_name = raw_name.trim();
            let (name, required) = match trimmed_name.strip_suffix('?') {
                Some(n) => (n.trim(), false),
                None => (trimmed_name, true),
            };
            if !name.is_empty() {
                let mut pd = CatalogParamDoc::new(name, raw_ty.trim(), "");
                pd.required = required;
                out.push(pd);
            }
        }
    }
    out
}

fn resolve_health(id: &str, health_map: &HashMap<String, String>) -> String {
    health_map
        .get(id)
        .filter(|s| !s.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| "ok".to_owned())
}

fn to_catalog_hit(entry: &CatalogEntry, health_map: &HashMap<String, String>) -> CatalogHit {
    let mut hit = CatalogHit::new(&entry.id, entry.kind.as_str(), &entry.summary);
    hit.signature = entry.signature.clone();
    hit.example = entry.example.clone();
    hit.auth = entry.auth.clone();
    hit.tier = entry.tier;
    hit.health = if entry.kind == EntryKind::Source {
        resolve_health(&entry.id, health_map)
    } else {
        "ok".to_owned()
    };
    hit
}

fn to_catalog_detail(entry: &CatalogEntry, health_map: &HashMap<String, String>) -> CatalogDetail {
    let mut detail = CatalogDetail::new(to_catalog_hit(entry, health_map));
    detail.params = entry.params.clone();
    detail.scopes = entry.scopes.clone();
    detail.caps = entry.caps.clone();
    detail.home_url = entry.home_url.clone();
    detail
}

fn describe_source_op(
    decl: &SourceDecl,
    op: &OpDecl,
    health_map: &HashMap<String, String>,
) -> CatalogDetail {
    let op_id = format!("{}.{}", decl.id, op.name);
    let summary = if op.summary.is_empty() {
        decl.summary.clone()
    } else {
        op.summary.clone()
    };
    let mut hit = CatalogHit::new(op_id, "source", summary);
    hit.signature = format!(
        "source.call(\"{}.{}\", {{ {} }})",
        decl.id,
        op.name,
        format_params_ts(op)
    );
    hit.example = format_source_example(decl, Some(op));
    hit.auth = match op.auth {
        OpAuthOverride::None => "keyless".to_owned(),
        _ => format_source_auth(decl).0,
    };
    hit.tier = decl.tier;
    hit.health = resolve_health(&decl.id, health_map);

    let mut detail = CatalogDetail::new(hit);
    detail.params = op.params.iter().map(param_to_doc).collect();
    detail.scopes = decl.scopes.clone();
    detail.caps = decl.caps.iter().map(|c| c.as_str().to_owned()).collect();
    detail.home_url = decl.home_url.clone();
    detail
}

fn describe_protocol_op(proto: &dyn ProtocolAdapter, op: &OpDecl) -> CatalogDetail {
    let kind = proto.kind();
    let op_id = format!("{kind}.{}", op.name);
    let mut hit = CatalogHit::new(op_id, "protocol", &op.summary);
    hit.signature = format!(
        "source.protocol({kind:?}, baseUrl: string, {:?}, {{ {} }})",
        op.name,
        format_params_ts(op)
    );
    hit.example = format!(
        "source.protocol({kind:?}, \"https://example.org\", {:?}, {})",
        op.name,
        sample_params_json(op)
    );
    hit.auth = "keyless".to_owned();
    hit.tier = 1;
    hit.health = "ok".to_owned();

    let mut detail = CatalogDetail::new(hit);
    detail.params = op.params.iter().map(param_to_doc).collect();
    detail.scopes = vec!["protocol".to_owned()];
    detail.caps = vec![op.name.clone()];
    detail
}

pub(crate) fn build_health_map(records: Vec<HealthRecord>) -> HashMap<String, String> {
    records
        .into_iter()
        .map(|r| (r.source_id, r.state))
        .collect()
}
