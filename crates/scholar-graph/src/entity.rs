//! Academic entity lookups (`ScholarGraph::entity`, `SPEC.md` §3.2, §6, M13).
//!
//! Dispatches strictly through `Arc<dyn SourcesApi>` to cataloged manifests:
//! - `EntityKind::Author` → `orcid` (`orcid.author`, `orcid.search`), `dblp` (`dblp.author`) → [`AuthorProfile`]
//! - `EntityKind::Org` → `ror` (`ror.org`, `ror.affiliation`, `ror.search`) → [`OrgProfile`]
//! - `EntityKind::Venue` → `dblp` (`dblp.venue`), `icore_ranks` (`icore_ranks.rank`),
//!   optional `issn_portal` (`issn_portal.lookup`) & `doaj` (`doaj.journals`) → [`VenueProfile`]
//! - `EntityKind::Grants` → `nih_reporter`, `nsf_awards`, `cordis`, `ukri` (`<src>.search`) → `Vec<GrantRecord>`

use crate::ids::{
    TITLE_MATCH_MIN, default_process, extract_issn, extract_orcid, extract_ror_id, title_score,
};
use fsm_core::{
    AuthorProfile, CallCx, EntityKind, FsmError, GrantRecord, OrgProfile, SourcesApi, VenueProfile,
};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;

const DEFAULT_GRANT_SOURCES: [&str; 4] = ["nih_reporter", "nsf_awards", "cordis", "ukri"];
const DEFAULT_GRANTS_LIMIT: usize = 10;
const MAX_GRANTS_LIMIT: usize = 50;

/// Core `ScholarGraph::entity` implementation.
pub async fn run_entity(
    sources: &Arc<dyn SourcesApi>,
    kind: EntityKind,
    query: &str,
    opts: Value,
    cx: &CallCx,
) -> Result<Value, FsmError> {
    let q = query.trim();
    if q.is_empty() {
        return Err(FsmError::invalid("entity query must not be empty"));
    }
    if cx.cancel.is_cancelled() {
        return Err(FsmError::Timeout(0));
    }

    match kind {
        EntityKind::Author => {
            let profile = lookup_author(sources, q, cx).await?;
            serde_json::to_value(profile)
                .map_err(|e| FsmError::internal("entity_ser", e.to_string()))
        }
        EntityKind::Org => {
            let profile = lookup_org(sources, q, &opts, cx).await?;
            serde_json::to_value(profile)
                .map_err(|e| FsmError::internal("entity_ser", e.to_string()))
        }
        EntityKind::Venue => {
            let profile = lookup_venue(sources, q, cx).await?;
            serde_json::to_value(profile)
                .map_err(|e| FsmError::internal("entity_ser", e.to_string()))
        }
        EntityKind::Grants => {
            let grants = lookup_grants(sources, q, &opts, cx).await?;
            serde_json::to_value(grants)
                .map_err(|e| FsmError::internal("entity_ser", e.to_string()))
        }
        _ => Err(FsmError::invalid("unsupported EntityKind variant")),
    }
}

// ---------------------------------------------------------------------------
// Author (`orcid` + `dblp`) -> AuthorProfile
// ---------------------------------------------------------------------------

async fn lookup_author(
    sources: &Arc<dyn SourcesApi>,
    query: &str,
    cx: &CallCx,
) -> Result<AuthorProfile, FsmError> {
    if let Some(orcid) = extract_orcid(query) {
        let out = sources
            .call("orcid.author", json!({ "orcid": orcid }), cx)
            .await?;
        if let Some(mut profile) = out.items.into_iter().find_map(parse_author_profile) {
            if profile.orcid.is_none() {
                profile.orcid = Some(orcid);
            }
            ensure_source(&mut profile.sources, "orcid");
            // Optionally enrich with DBLP author counts ONLY when the candidate
            // refers to the same researcher (matching ORCID, or no ORCID + exact name).
            if !profile.name.is_empty()
                && let Ok(dblp_out) = sources
                    .call(
                        "dblp.author",
                        json!({ "query": profile.name, "limit": 5 }),
                        cx,
                    )
                    .await
            {
                let dblp_profiles: Vec<AuthorProfile> = dblp_out
                    .items
                    .into_iter()
                    .filter_map(parse_author_profile)
                    .collect();
                if let Some(matched) = pick_merge_author_candidate(
                    &profile.name,
                    profile.orcid.as_deref(),
                    dblp_profiles,
                ) {
                    merge_author_profile(&mut profile, matched);
                }
            }
            return Ok(profile);
        }
        let mut empty = AuthorProfile::new(query);
        empty.orcid = Some(orcid);
        return Ok(empty);
    }

    let (orcid_res, dblp_res) = tokio::join!(
        sources.call("orcid.search", json!({ "query": query, "limit": 5 }), cx),
        sources.call("dblp.author", json!({ "query": query, "limit": 5 }), cx),
    );

    if let (Err(e1), Err(_)) = (&orcid_res, &dblp_res) {
        return Err(clone_error_or_upstream("orcid", e1));
    }

    let orcid_profiles: Vec<AuthorProfile> = orcid_res
        .map(|o| {
            o.items
                .into_iter()
                .filter_map(parse_author_profile)
                .collect()
        })
        .unwrap_or_default();
    let dblp_profiles: Vec<AuthorProfile> = dblp_res
        .map(|o| {
            o.items
                .into_iter()
                .filter_map(parse_author_profile)
                .collect()
        })
        .unwrap_or_default();

    let primary = pick_matching_author(query, orcid_profiles);

    if let Some(mut p) = primary {
        // If ORCID search returned a candidate with an ORCID iD but no full record yet,
        // hydrate via `orcid.author` to pull `worksCount`, verified affiliations, and `topWorks`.
        if let Some(ref oid) = p.orcid.clone()
            && p.top_works.is_empty()
            && p.works_count.is_none()
            && let Ok(full_out) = sources
                .call("orcid.author", json!({ "orcid": oid }), cx)
                .await
            && let Some(full) = full_out.items.into_iter().find_map(parse_author_profile)
        {
            merge_author_profile(&mut p, full);
        }

        if let Some(dblp_match) =
            pick_merge_author_candidate(&p.name, p.orcid.as_deref(), dblp_profiles)
        {
            merge_author_profile(&mut p, dblp_match);
        }
        return Ok(p);
    }

    if let Some(b) = pick_matching_author(query, dblp_profiles) {
        return Ok(b);
    }
    Ok(AuthorProfile::new(query))
}

fn parse_author_profile(val: Value) -> Option<AuthorProfile> {
    if let Ok(p) = serde_json::from_value::<AuthorProfile>(val.clone())
        && (!p.name.trim().is_empty() || p.orcid.is_some())
    {
        return Some(p);
    }
    let name = val.get("name").and_then(Value::as_str)?.trim();
    if name.is_empty() {
        return None;
    }
    let mut p = AuthorProfile::new(name);
    p.orcid = val
        .get("orcid")
        .and_then(Value::as_str)
        .and_then(extract_orcid);
    p.openalex_id = val
        .get("openalexId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(arr) = val.get("affiliations").and_then(Value::as_array) {
        p.affiliations = arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
    }
    p.works_count = val.get("worksCount").and_then(Value::as_u64);
    p.cited_by_count = val.get("citedByCount").and_then(Value::as_u64);
    p.h_index = val
        .get("hIndex")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok());
    if let Some(arr) = val.get("sources").and_then(Value::as_array) {
        p.sources = arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
    }
    Some(p)
}

fn pick_matching_author(query: &str, candidates: Vec<AuthorProfile>) -> Option<AuthorProfile> {
    if candidates.is_empty() {
        return None;
    }
    let mut best_idx = 0usize;
    let mut best_score = -1.0f64;
    for (i, c) in candidates.iter().enumerate() {
        let s = title_score(query, &c.name);
        if s > best_score {
            best_score = s;
            best_idx = i;
        }
    }
    candidates.into_iter().nth(best_idx)
}

/// Strict cross-source merge matcher (I2): a DBLP candidate is merged into an
/// existing author profile ONLY if:
/// - (a) both have ORCIDs and they are equal (case-insensitive), OR
/// - (b) the candidate has NO ORCID and its normalized name equals the profile's
///   normalized name exactly (so disambiguated homonyms like `"Wei Wang 0001"`
///   or different-ORCID homonyms are never merged).
fn pick_merge_author_candidate(
    profile_name: &str,
    profile_orcid: Option<&str>,
    candidates: Vec<AuthorProfile>,
) -> Option<AuthorProfile> {
    if let Some(oid) = profile_orcid
        && let Some(pos) = candidates.iter().position(|c| {
            c.orcid
                .as_deref()
                .is_some_and(|x| x.eq_ignore_ascii_case(oid))
        })
    {
        return candidates.into_iter().nth(pos);
    }
    let norm_name = default_process(profile_name);
    if norm_name.is_empty() {
        return None;
    }
    candidates
        .into_iter()
        .find(|c| c.orcid.is_none() && default_process(&c.name) == norm_name)
}

fn merge_author_profile(dst: &mut AuthorProfile, src: AuthorProfile) {
    if dst.name.trim().is_empty() && !src.name.trim().is_empty() {
        dst.name = src.name;
    }
    if dst.orcid.is_none() {
        dst.orcid = src.orcid;
    }
    if dst.openalex_id.is_none() {
        dst.openalex_id = src.openalex_id;
    }
    for aff in src.affiliations {
        if !dst
            .affiliations
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&aff))
        {
            dst.affiliations.push(aff);
        }
    }
    dst.works_count = match (dst.works_count, src.works_count) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    dst.cited_by_count = match (dst.cited_by_count, src.cited_by_count) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    dst.h_index = match (dst.h_index, src.h_index) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    if dst.top_works.is_empty() {
        dst.top_works = src.top_works;
    }
    for s in src.sources {
        ensure_source(&mut dst.sources, &s);
    }
}

// ---------------------------------------------------------------------------
// Organization (`ror`) -> OrgProfile
// ---------------------------------------------------------------------------

async fn lookup_org(
    sources: &Arc<dyn SourcesApi>,
    query: &str,
    opts: &Value,
    cx: &CallCx,
) -> Result<OrgProfile, FsmError> {
    if let Some(ror_id) = extract_ror_id(query) {
        let out = sources.call("ror.org", json!({ "id": ror_id }), cx).await?;
        if let Some(mut org) = out.items.into_iter().find_map(parse_org_profile) {
            if org.ror.is_none() {
                org.ror = Some(format!("https://ror.org/{ror_id}"));
            }
            ensure_source(&mut org.sources, "ror");
            return Ok(org);
        }
        let mut empty = OrgProfile::new(query);
        empty.ror = Some(format!("https://ror.org/{ror_id}"));
        return Ok(empty);
    }

    let explicit_affiliation = opts
        .get("affiliation")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let looks_like_affiliation = explicit_affiliation || (query.contains(',') && query.len() > 25);

    if looks_like_affiliation
        && let Ok(out) = sources
            .call("ror.affiliation", json!({ "affiliation": query }), cx)
            .await
        && let Some(mut org) = out.items.into_iter().find_map(parse_org_profile)
    {
        ensure_source(&mut org.sources, "ror");
        return Ok(org);
    }

    let out = sources
        .call("ror.search", json!({ "query": query }), cx)
        .await?;
    let candidates: Vec<OrgProfile> = out
        .items
        .into_iter()
        .filter_map(parse_org_profile)
        .collect();
    if let Some(mut best) = pick_matching_org(query, candidates) {
        ensure_source(&mut best.sources, "ror");
        return Ok(best);
    }
    Ok(OrgProfile::new(query))
}

fn parse_org_profile(val: Value) -> Option<OrgProfile> {
    if let Ok(p) = serde_json::from_value::<OrgProfile>(val.clone())
        && (!p.name.trim().is_empty() || p.ror.is_some())
    {
        return Some(p);
    }
    let name = val.get("name").and_then(Value::as_str)?.trim();
    if name.is_empty() {
        return None;
    }
    let mut p = OrgProfile::new(name);
    p.ror = val.get("ror").and_then(Value::as_str).map(str::to_owned);
    p.country = val
        .get("country")
        .and_then(Value::as_str)
        .map(str::to_owned);
    p.org_type = val
        .get("orgType")
        .and_then(Value::as_str)
        .map(str::to_owned);
    p.homepage = val
        .get("homepage")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(p)
}

fn pick_matching_org(query: &str, candidates: Vec<OrgProfile>) -> Option<OrgProfile> {
    if candidates.is_empty() {
        return None;
    }
    let norm_q = default_process(query);
    if let Some(pos) = candidates
        .iter()
        .position(|c| default_process(&c.name) == norm_q)
    {
        return candidates.into_iter().nth(pos);
    }
    // Otherwise preserve ROR's relevance order (index 0).
    candidates.into_iter().next()
}

// ---------------------------------------------------------------------------
// Venue (`dblp` + `icore_ranks` + optional `issn_portal` & `doaj`) -> VenueProfile
// ---------------------------------------------------------------------------

async fn lookup_venue(
    sources: &Arc<dyn SourcesApi>,
    query: &str,
    cx: &CallCx,
) -> Result<VenueProfile, FsmError> {
    if let Some(issn) = extract_issn(query) {
        // I4: `dblp.venue` only matches titles via SPARQL `LCASE(?title)` and never
        // matches bare ISSNs; query `issn_portal.lookup` + `doaj.journals` directly.
        let (issn_res, doaj_res) = tokio::join!(
            sources.call("issn_portal.lookup", json!({ "issn": issn }), cx),
            sources.call(
                "doaj.journals",
                json!({ "query": format!("issn:{issn}"), "limit": 1 }),
                cx
            ),
        );

        if let (Err(e1), Err(_)) = (&issn_res, &doaj_res) {
            return Err(clone_error_or_upstream("issn_portal", e1));
        }

        let mut merged: Option<VenueProfile> = None;
        if let Ok(out) = issn_res
            && let Some(v) = out.items.into_iter().find_map(parse_venue_profile)
        {
            merged = Some(v);
        }
        if let Ok(out) = doaj_res {
            if let Some(mut doaj_prof) = out.items.into_iter().find_map(parse_venue_profile) {
                doaj_prof.doaj_vetted = Some(true);
                ensure_source(&mut doaj_prof.sources, "doaj");
                match merged.as_mut() {
                    Some(m) => merge_venue_profile(m, doaj_prof),
                    None => merged = Some(doaj_prof),
                }
            } else {
                let target = merged.get_or_insert_with(|| VenueProfile::new(query));
                if target.doaj_vetted.is_none() {
                    target.doaj_vetted = Some(false);
                }
            }
        }
        let mut profile = merged.unwrap_or_else(|| VenueProfile::new(query));
        if !profile.issn.contains(&issn) {
            profile.issn.push(issn);
        }
        return Ok(profile);
    }

    let (dblp_res, icore_res, doaj_res) = tokio::join!(
        sources.call("dblp.venue", json!({ "query": query, "limit": 5 }), cx),
        sources.call("icore_ranks.rank", json!({ "query": query }), cx),
        sources.call("doaj.journals", json!({ "query": query, "limit": 3 }), cx),
    );

    if let (Err(e1), Err(_), Err(_)) = (&dblp_res, &icore_res, &doaj_res) {
        return Err(clone_error_or_upstream("dblp", e1));
    }

    let dblp_candidates: Vec<VenueProfile> = dblp_res
        .map(|o| {
            o.items
                .into_iter()
                .filter_map(parse_venue_profile)
                .collect()
        })
        .unwrap_or_default();
    let dblp_match = pick_matching_venue(query, dblp_candidates);
    let icore_match = icore_res.ok().and_then(|o| {
        pick_icore_venue(query, dblp_match.as_ref().map(|d| d.name.as_str()), o.items)
    });

    let mut merged = match (dblp_match, icore_match) {
        (Some(mut d), Some(ic)) => {
            // B2: Only attach an ICORE rank when the ICORE venue genuinely refers to the
            // same conference as DBLP (never attach a bogus rank from an unrelated row).
            if venue_names_compatible(query, &d.name, &ic.name) {
                merge_venue_profile(&mut d, ic);
            }
            Some(d)
        }
        (Some(d), None) => Some(d),
        (None, Some(ic)) => Some(ic),
        (None, None) => None,
    };

    if let Ok(out) = doaj_res
        && !out.items.is_empty()
    {
        let norm_q = default_process(query);
        let exact_doaj = out.items.into_iter().find_map(|item| {
            let mut prof = parse_venue_profile(item)?;
            if default_process(&prof.name) == norm_q {
                prof.doaj_vetted = Some(true);
                ensure_source(&mut prof.sources, "doaj");
                Some(prof)
            } else {
                None
            }
        });
        if let Some(doaj_prof) = exact_doaj {
            match merged.as_mut() {
                Some(m) => merge_venue_profile(m, doaj_prof),
                None => merged = Some(doaj_prof),
            }
        }
    }

    Ok(merged.unwrap_or_else(|| VenueProfile::new(query)))
}

fn parse_venue_profile(val: Value) -> Option<VenueProfile> {
    if let Ok(mut p) = serde_json::from_value::<VenueProfile>(val.clone())
        && !p.name.trim().is_empty()
    {
        p.name = p.name.trim().to_owned();
        return Some(p);
    }
    let name = val
        .get("name")
        .or_else(|| val.get("title"))
        .and_then(Value::as_str)?
        .trim();
    if name.is_empty() {
        return None;
    }
    let mut p = VenueProfile::new(name);
    if let Some(arr) = val.get("issn").and_then(Value::as_array) {
        p.issn = arr
            .iter()
            .filter_map(Value::as_str)
            .filter_map(extract_issn)
            .collect();
    }
    p.publisher = val
        .get("publisher")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    p.doaj_vetted = val.get("doajVetted").and_then(Value::as_bool);
    p.icore_rank = val
        .get("icoreRank")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    p.openalex_id = val
        .get("openalexId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    p.works_count = val.get("worksCount").and_then(Value::as_u64);
    if let Some(arr) = val.get("sources").and_then(Value::as_array) {
        p.sources = arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
    }
    Some(p)
}

/// Compute `(all_initials, content_initials)` of a venue title, e.g.
/// `"International Conference on Machine Learning"` → `("icoml", "icml")`.
fn title_initials(title: &str) -> (String, String) {
    const STOPWORDS: &[&str] = &[
        "on", "of", "in", "for", "the", "and", "to", "a", "an", "at", "by", "with",
    ];
    let norm = default_process(title);
    let mut all = String::new();
    let mut content = String::new();
    for w in norm.split_whitespace() {
        if let Some(ch) = w.chars().next() {
            all.push(ch);
            if !STOPWORDS.contains(&w) {
                content.push(ch);
            }
        }
    }
    (all, content)
}

fn title_matches_initials(title: &str, norm_q: &str) -> bool {
    if norm_q.len() < 2 || norm_q.contains(' ') {
        return false;
    }
    let (all, content) = title_initials(title);
    content == norm_q || all == norm_q
}

fn venue_names_compatible(query: &str, dblp_name: &str, icore_name: &str) -> bool {
    let nd = default_process(dblp_name);
    let ni = default_process(icore_name);
    if !nd.is_empty() && nd == ni {
        return true;
    }
    let nq = default_process(query);
    if title_matches_initials(dblp_name, &nq) && title_matches_initials(icore_name, &nq) {
        return true;
    }
    title_score(dblp_name, icore_name) >= TITLE_MATCH_MIN
}

/// Select the best matching ICORE row (B2):
/// 1. Exact `acronym` field match (when present in raw rows)
/// 2. Exact normalized `name == query`
/// 3. Exact normalized `name == dblp_name` (handles promoted `icore_ranks.toml`, which strips `acronym`)
/// 4. Title initials match (`content_initials` or `all_initials == norm_q`, e.g. ICML → "International Conference on Machine Learning")
/// 5. Highest `title_score`, breaking ties by shortest title name and earliest index (never `max_by`'s last-wins).
fn pick_icore_venue(
    query: &str,
    dblp_name: Option<&str>,
    items: Vec<Value>,
) -> Option<VenueProfile> {
    if items.is_empty() {
        return None;
    }
    let norm_q = default_process(query);
    let norm_dblp = dblp_name.map(default_process).filter(|s| !s.is_empty());

    let parsed: Vec<(usize, Option<String>, VenueProfile)> = items
        .into_iter()
        .enumerate()
        .filter_map(|(i, row)| {
            let acronym = row
                .get("acronym")
                .and_then(Value::as_str)
                .map(default_process)
                .filter(|s| !s.is_empty());
            let mut profile = parse_venue_profile(row)?;
            ensure_source(&mut profile.sources, "icore_ranks");
            Some((i, acronym, profile))
        })
        .collect();
    if parsed.is_empty() {
        return None;
    }

    // 1. Exact acronym match (shortest name, then earliest index).
    if let Some((_, _, p)) = parsed
        .iter()
        .filter(|(_, acr, _)| acr.as_deref() == Some(norm_q.as_str()))
        .min_by_key(|(i, _, p)| (p.name.len(), *i))
    {
        return Some(p.clone());
    }

    // 2. Exact title match to query.
    if let Some((_, _, p)) = parsed
        .iter()
        .find(|(_, _, p)| default_process(&p.name) == norm_q)
    {
        return Some(p.clone());
    }

    // 3. Exact title match to DBLP's resolved venue name.
    if let Some(ref nd) = norm_dblp
        && let Some((_, _, p)) = parsed
            .iter()
            .find(|(_, _, p)| default_process(&p.name) == *nd)
    {
        return Some(p.clone());
    }

    // 4. Initials match (e.g. "icml" -> "International Conference on Machine Learning").
    if let Some((_, _, p)) = parsed
        .iter()
        .filter(|(_, _, p)| title_matches_initials(&p.name, &norm_q))
        .min_by_key(|(i, _, p)| (p.name.len(), *i))
    {
        return Some(p.clone());
    }

    // 5. Highest title_score, breaking ties by shortest title and earliest index.
    let mut best_idx = 0usize;
    let mut best_score = -1.0f64;
    let mut best_len = usize::MAX;
    let mut best_pos = usize::MAX;
    for (idx, (orig_pos, _, p)) in parsed.iter().enumerate() {
        let mut s = title_score(query, &p.name);
        if let Some(dn) = dblp_name {
            s = s.max(title_score(dn, &p.name));
        }
        let len = p.name.len();
        let better = if (s - best_score).abs() > 1e-9 {
            s > best_score
        } else if len != best_len {
            len < best_len
        } else {
            *orig_pos < best_pos
        };
        if better {
            best_idx = idx;
            best_score = s;
            best_len = len;
            best_pos = *orig_pos;
        }
    }
    Some(parsed[best_idx].2.clone())
}

fn pick_matching_venue(query: &str, candidates: Vec<VenueProfile>) -> Option<VenueProfile> {
    if candidates.is_empty() {
        return None;
    }
    let norm_q = default_process(query);
    if let Some(pos) = candidates
        .iter()
        .position(|c| default_process(&c.name) == norm_q)
    {
        return candidates.into_iter().nth(pos);
    }
    let mut best_idx = 0usize;
    let mut best_score = -1.0f64;
    for (i, c) in candidates.iter().enumerate() {
        let s = title_score(query, &c.name);
        if s > best_score {
            best_score = s;
            best_idx = i;
        }
    }
    candidates.into_iter().nth(best_idx)
}

fn merge_venue_profile(dst: &mut VenueProfile, src: VenueProfile) {
    if dst.name.trim().is_empty() && !src.name.trim().is_empty() {
        dst.name = src.name;
    }
    for issn in src.issn {
        if !dst.issn.contains(&issn) {
            dst.issn.push(issn);
        }
    }
    if dst.publisher.is_none() {
        dst.publisher = src.publisher;
    }
    if dst.doaj_vetted.is_none() {
        dst.doaj_vetted = src.doaj_vetted;
    }
    if dst.icore_rank.is_none() {
        dst.icore_rank = src.icore_rank;
    }
    if dst.openalex_id.is_none() {
        dst.openalex_id = src.openalex_id;
    }
    dst.works_count = match (dst.works_count, src.works_count) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    for s in src.sources {
        ensure_source(&mut dst.sources, &s);
    }
}

// ---------------------------------------------------------------------------
// Grants (`nih_reporter`, `nsf_awards`, `cordis`, `ukri`) -> Vec<GrantRecord>
// ---------------------------------------------------------------------------

/// Per-source `(min, max)` `limit` bounds matching the staged grant manifests:
/// - `nih_reporter`: `1..=50`
/// - `nsf_awards`: `1..=25`
/// - `cordis`: `1..=50`
/// - `ukri`: `10..=100` (UKRI GtR rejects page sizes below 10 with HTTP 400)
fn known_grant_limit_bounds(source_id: &str) -> Option<(usize, usize)> {
    match source_id {
        "nih_reporter" => Some((1, 50)),
        "nsf_awards" => Some((1, 25)),
        "cordis" => Some((1, 50)),
        "ukri" => Some((10, 100)),
        _ => None,
    }
}

async fn lookup_grants(
    sources: &Arc<dyn SourcesApi>,
    query: &str,
    opts: &Value,
    cx: &CallCx,
) -> Result<Vec<GrantRecord>, FsmError> {
    let limit = opts
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_GRANTS_LIMIT, |n| {
            (n as usize).clamp(1, MAX_GRANTS_LIMIT)
        });

    let requested_sources: Vec<(String, usize, usize)> = match opts.get("sources") {
        None | Some(Value::Null) => DEFAULT_GRANT_SOURCES
            .iter()
            .map(|&s| {
                let (lo, hi) = known_grant_limit_bounds(s).unwrap_or((1, MAX_GRANTS_LIMIT));
                (s.to_owned(), lo, hi)
            })
            .collect(),
        Some(Value::Array(arr)) if arr.is_empty() => DEFAULT_GRANT_SOURCES
            .iter()
            .map(|&s| {
                let (lo, hi) = known_grant_limit_bounds(s).unwrap_or((1, MAX_GRANTS_LIMIT));
                (s.to_owned(), lo, hi)
            })
            .collect(),
        Some(Value::Array(arr)) => {
            let mut seen = HashSet::new();
            let mut validated = Vec::new();
            for item in arr {
                let Some(raw) = item.as_str() else {
                    return Err(FsmError::invalid(
                        "opts.sources entries must be non-empty grant source id strings",
                    ));
                };
                let norm = raw.trim().to_ascii_lowercase();
                if norm.is_empty() {
                    return Err(FsmError::invalid(
                        "opts.sources entries must not be empty strings",
                    ));
                }
                if !seen.insert(norm.clone()) {
                    continue;
                }
                if let Some((lo, hi)) = known_grant_limit_bounds(&norm) {
                    validated.push((norm, lo, hi));
                } else {
                    // Allow cataloged third-party sources only if they explicitly declare the `grant` cap.
                    let detail = sources.catalog_describe(&norm).await.ok();
                    let has_grant_cap = detail
                        .as_ref()
                        .is_some_and(|d| d.caps.iter().any(|c| c.eq_ignore_ascii_case("grant")));
                    if !has_grant_cap {
                        return Err(FsmError::invalid(format!(
                            "unknown or non-grant source '{norm}'; allowed grant sources: {}",
                            DEFAULT_GRANT_SOURCES.join(", ")
                        )));
                    }
                    let (lo, hi) = detail
                        .as_ref()
                        .and_then(|d| d.params.iter().find(|p| p.name == "limit"))
                        .map_or((1, MAX_GRANTS_LIMIT), |p| {
                            let lo = p.min.map_or(1, |v| v.max(1.0) as usize);
                            let hi = p
                                .max
                                .map_or(MAX_GRANTS_LIMIT, |v| v.max(lo as f64) as usize);
                            (lo, hi)
                        });
                    validated.push((norm, lo, hi));
                }
            }
            validated
        }
        Some(_) => {
            return Err(FsmError::invalid(
                "opts.sources must be an array of grant source ids",
            ));
        }
    };

    let futs = requested_sources.iter().map(|(src_id, min_lim, max_lim)| {
        let src_limit = limit.clamp(*min_lim, *max_lim);
        let op_id = format!("{src_id}.search");
        let params = json!({ "query": query, "limit": src_limit });
        async move {
            let res = sources.call(&op_id, params, cx).await;
            (src_id.clone(), res)
        }
    });

    let results = futures::future::join_all(futs).await;

    let mut per_source: Vec<Vec<GrantRecord>> = Vec::new();
    let mut any_ok = false;
    let mut first_err: Option<(String, FsmError)> = None;

    for (src_id, res) in results {
        match res {
            Ok(out) => {
                any_ok = true;
                let records: Vec<GrantRecord> = out
                    .items
                    .into_iter()
                    .filter_map(|v| parse_grant_record(v, &src_id))
                    .collect();
                if !records.is_empty() {
                    per_source.push(records);
                }
            }
            Err(e) => {
                tracing::debug!(source = %src_id, error = %e, "grant source call failed");
                if first_err.is_none() {
                    first_err = Some((src_id, e));
                }
            }
        }
    }

    if !any_ok && let Some((src_id, err)) = first_err {
        return Err(clone_error_or_upstream(&src_id, &err));
    }

    // Round-robin interleave across grant funders and deduplicate by `(source, id)`.
    let mut out = Vec::with_capacity(limit);
    let mut seen: HashSet<String> = HashSet::new();
    let mut round = 0usize;
    while out.len() < limit {
        let mut progressed = false;
        for bucket in &per_source {
            if let Some(rec) = bucket.get(round) {
                progressed = true;
                let key = if !rec.id.trim().is_empty() {
                    format!("{}:{}", rec.source, rec.id.trim().to_ascii_lowercase())
                } else {
                    format!(
                        "{}:{}",
                        rec.funder.to_ascii_lowercase(),
                        default_process(&rec.title)
                    )
                };
                if seen.insert(key) {
                    out.push(rec.clone());
                    if out.len() >= limit {
                        break;
                    }
                }
            }
        }
        if !progressed {
            break;
        }
        round += 1;
    }

    Ok(out)
}

fn parse_grant_record(val: Value, fallback_source: &str) -> Option<GrantRecord> {
    if let Ok(mut g) = serde_json::from_value::<GrantRecord>(val.clone())
        && (!g.title.trim().is_empty() || !g.id.trim().is_empty())
    {
        if g.source.trim().is_empty() {
            g.source = fallback_source.to_owned();
        }
        return Some(g);
    }
    let id = val
        .get("id")
        .and_then(|v| {
            v.as_str()
                .map(str::to_owned)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
        })
        .unwrap_or_default();
    let title = val
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if id.is_empty() && title.is_empty() {
        return None;
    }
    let funder = val
        .get("funder")
        .and_then(Value::as_str)
        .unwrap_or(fallback_source)
        .to_owned();
    let source = val
        .get("source")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_source);
    let mut g = GrantRecord::new(id, title, funder, source);
    g.pi = val.get("pi").and_then(Value::as_str).map(str::to_owned);
    g.org = val.get("org").and_then(Value::as_str).map(str::to_owned);
    g.amount = val.get("amount").and_then(Value::as_f64);
    g.currency = val
        .get("currency")
        .and_then(Value::as_str)
        .map(str::to_owned);
    g.start_date = val
        .get("startDate")
        .and_then(Value::as_str)
        .map(str::to_owned);
    g.end_date = val
        .get("endDate")
        .and_then(Value::as_str)
        .map(str::to_owned);
    g.url = val.get("url").and_then(Value::as_str).map(str::to_owned);
    Some(g)
}

fn ensure_source(sources: &mut Vec<String>, name: &str) {
    if !sources.iter().any(|s| s == name) {
        sources.push(name.to_owned());
    }
}

fn clone_error_or_upstream(source_id: &str, err: &FsmError) -> FsmError {
    match err {
        FsmError::InvalidInput(m) => FsmError::InvalidInput(m.clone()),
        FsmError::Timeout(ms) => FsmError::Timeout(*ms),
        FsmError::PermissionDenied(m) => FsmError::PermissionDenied(m.clone()),
        FsmError::RateLimited {
            source_id,
            retry_after_s,
        } => FsmError::RateLimited {
            source_id: source_id.clone(),
            retry_after_s: *retry_after_s,
        },
        FsmError::AuthRequired { source_id, env_var } => FsmError::AuthRequired {
            source_id: source_id.clone(),
            env_var: env_var.clone(),
        },
        FsmError::Upstream {
            source_id,
            status,
            message,
        } => FsmError::Upstream {
            source_id: source_id.clone(),
            status: *status,
            message: message.clone(),
        },
        _ => FsmError::upstream(source_id, None, err.to_string()),
    }
}
