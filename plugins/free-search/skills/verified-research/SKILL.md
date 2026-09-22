---
name: verified-research
description: "Use with the free-search MCP tools whenever a web lookup must yield facts someone will rely on: dates, deadlines, prices, prizes, fees, rules, eligibility, schedules, versions, statistics, news, or anything that may have changed. Search results only locate sources; this workflow opens the official or primary page, checks publish date and cache age against today, confirms key facts with a second independent source, and reports dated citations plus what could not be verified. 中文触发：查官网、核实信息、最新消息、报名或截止时间、赛事组别/赛区/奖金、价格、政策、版本号、是否过期或仍有效。"
---

# Verified research with free-search

Use search results to find pages, and take facts only from the pages. A snippet
is an engine's summary of a page as it looked when it was crawled: it drops
qualifiers, merges editions, and is often years old. Use this workflow whenever
someone will rely on the answer.

Tool names below are the bare names on the `search` MCP server; your host may
show them with a prefix.

## 0. Fix today's date

Know today's date before judging anything as current. Take it from your system
context. Failing that, the `retrieved …` stamp on a fresh (uncached) `search`
result is the server's clock. "Latest", "this year" and "upcoming" are relative
to it. Do not infer the current year from search results or from memory.

## 1. Locate: `search` is for URLs only

- `search(query)` finds candidate pages. Add `freshness="month"` (or
  `"day"`/`"week"`/`"year"`) when recency matters, `include_domains=[...]` once
  you know the official domain, `category=` for papers, filings, news, datasets.
- Some facts have a registry, and the search asks it on its own when the
  question names the thing plainly: a package ("latest fastapi version"), a
  CVE id, two currencies ("100 usd to cny"), a place and a weather word, a
  country and an indicator ("china gdp"), a city and a time word, a domain,
  a coin, an iOS app. The record comes back as the first result with the
  publisher's date, marked "Record sources", and can be cited as it is.
  `category=` ("software", "security", "weather", "finance.fx", "stats",
  "calendar", "reference") asks the same sources explicitly.
- Pick the primary source: the organizer's, vendor's, regulator's or author's
  own page, the filing, the paper, the repository. Aggregators, listicles,
  reposts, forums and AI-written summaries are useful only for finding it.
- Never copy a date, amount, name, rule or number from a snippet or the `Lead:`
  line into your answer.
- Search in the language of the source. A Chinese query is routed to a Chinese
  index automatically; to force more of them use `engines=["so360", "baidu"]`.

## 2. Read the source itself

| Need | Call |
|---|---|
| One page | `fetch(url)` |
| Several candidates | `fetch_batch(urls)` |
| Find and read in one round trip | `research(question, depth=3, include_domains=[...])` |
| Long PDF or DOCX (rules, notices) | `read_doc(source, start, length)`, next page at `start + returned_chars` |
| Dates, prices, event metadata as fields | `extract_structured(url)`: JSON-LD `startDate`, `price`, `datePublished`, `dateModified` |
| One question across 2-5 pages | `compare(question, urls)` |
| A paper you are about to cite | `paper_graph(paper)` for retraction and correction notices |

Divisions, regions, fee tables, prize breakdowns and deadlines usually sit on a
sub-page or in a linked PDF (rules, FAQ, 章程, 通知). If the landing page lacks
them, follow its links with `fetch` or `read_doc` before you go back to search.
If `fetch` returns a near-empty body, retry with `render="browser"`.

## 3. Check that it is current

For every source you rely on, establish three things.

1. When it was published or last updated. Look for `published <date>` in a
   `fetch` header, `datePublished`/`dateModified` from `extract_structured`, or
   a date in the body. `no publication date found` means the page is undated.
   Say so, and do not assume it is recent.
2. When this copy was retrieved. Pages and searches are cached for up to 7
   days, and `cached N days ago` in the output means you are reading a stored
   copy. For anything that changes (deadlines, prices, availability, standings,
   versions, breaking news) pull it again with `fetch(url, force_refresh=True)`,
   or `max_age_hours=0` on `search` and `research`.
3. Which edition it describes. The year, season, version or round on the page
   must match the one asked about. Last year's page for an annual event is the
   most common wrong answer, because it is official and detailed and so looks
   right. If only an older edition is published, report it as that edition and
   say the current one is not announced.

`freshness=` is best-effort and keeps undated results, so a result that
survives `freshness="month"` may still be old. The search says how many results
it could date (`dated: 3/10`) and marks each one.

## 4. Corroborate

- A fact the user will act on needs one primary source, or two independent
  ones. Independent means different origin: two outlets reprinting one press
  release or wire story count as one source. Identical wording usually means a
  shared origin.
- Prefer primary over secondary, and the newer of two primary pages.
- When sources disagree, do not average and do not pick silently. Report both
  values, who says which, their dates, and which you treat as authoritative
  and why.

## 5. Read the warnings

- `dated <date>` on a result means the date came from a feed or an API field.
  `<date> (from snippet text)` means it was lifted out of prose and may be a
  date the page mentions, which can differ from the page's own. `undated` means
  the age is unknown. Treat it as possibly old.
- "Undated results": most results carried no date although you asked for
  recency. Confirm dates on the pages themselves.
- "Gated engines", "Silent engines", "Rate-limited engines", "Benched engines",
  `rescued via` and `requested but contributed nothing` all mean coverage was
  partial. An empty result does not show that nothing exists. Rephrase or
  change engines before concluding.
- An engine that "answered with results that match only the first word of the
  query" had those results discarded. You need not act on it, but coverage was
  narrower.
- The kind shown on a result (`government`, `academic`, `news`, `paper`,
  `forum`, `code`) describes the site. It says nothing about quality.
- An engine error about a missing API key: ignore it and continue with the
  keyless engines. Never ask the user for a key.

## 6. Report

- Give each fact with source and date, e.g. "Registration closes 2026-10-15
  (organizer page, updated 2026-09-02, retrieved today)" with the URL.
- Separate what you read on a primary page from what only secondary sources say.
- End with a "Could not verify" list: every requested detail you did not find
  on a page you actually read, and every unresolved conflict. Never fill a gap
  from a snippet, from memory, or by inference from an earlier edition.
- If nothing authoritative was reachable, say so. Do not fall back silently to
  snippet-based answers.

## Cost

A normal verification is one `search`, one or two `fetch` calls on the primary
page, and one corroborating source. Use `research(question, fetch=False)` to
choose what to read, keep `depth` at 2-3, and use `cache_search` to find a page
you already read this session.
