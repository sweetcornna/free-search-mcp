//! MCP tool definitions (`search`, `exec`, `ask`) and server instructions (`SPEC.md` §2.2–§2.4, ruling R5).

use crate::host_api::render_dts_namespaces;
use serde_json::{Value, json};

/// MCP `ServerInfo.instructions` (`SPEC.md` §2.2, ruling R5; hard gate <= 450 bytes).
const SERVER_INSTRUCTIONS: &str = "Use `ask` for small factual lookups (versions, dates, prices, definitions, one-off questions). Use `search` for multi-engine discovery (live verticals, `cache`, or 300+ source `catalog`). Use `exec` to fetch pages, read docs, query scholarly graphs/full text, or run multi-step research. Search snippets are summaries: read pages via `web.fetch` before relying on exact numbers or rules.";

const SEARCH_TITLE: &str = "Search web, verticals, cache, or catalog";
const EXEC_TITLE: &str = "Run sandboxed JS with web/scholar/doc/source/cache APIs";
const ASK_TITLE: &str = "Quick factual lookup with cited passages or agent";

const SEARCH_DESCRIPTION: &str = "Keyless search over live verticals, FTS5 cache, or 300+ source catalog (use `ask` for small lookups).\n\
- Live: query + scope ('web','academic','news','code','docs','dataset','finance','security','gov','media','forum','reference', or sub-scopes). Direct registries auto-route on 'web'.\n\
- Catalog (0 net): scope='catalog' finds 300+ sources, 18 protocols, exec APIs.\n\
- Cache (0 net): scope='cache' searches fetched pages.\n\
Snippets are summaries: fetch pages via exec for exact dates/numbers/rules.";

const EXEC_DESCRIPTION_PREFIX: &str = "Sandboxed ES2020 JS (QuickJS-NG, no fs/env/sockets) for research, many sources, or custom logic (use `ask` for small lookups). Pass async arrow, statements+return, or expression; use Promise.all. Find 300+ sources via catalog.search(q).\n\n```ts\n";

const ASK_DESCRIPTION: &str = "Small factual lookups: versions, dates, prices, definitions, one-off questions; use search/exec for research, many sources or custom logic. Answers from direct registries or top pages with citations (or answer agent if set).";

/// MCP tool annotations (`SPEC.md` §2.3–§2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolAnnotations {
    pub read_only_hint: bool,
    pub destructive_hint: bool,
    pub idempotent_hint: bool,
    pub open_world_hint: bool,
}

/// One MCP tool definition exposed by `free-search-mcp`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: String,
    pub input_schema: Value,
}

impl ToolDef {
    /// Human-readable MCP `Tool.title` (`SPEC.md` §2.3–§2.4, ruling R5).
    pub fn title(&self) -> &'static str {
        match self.name {
            "search" => SEARCH_TITLE,
            "exec" => EXEC_TITLE,
            "ask" => ASK_TITLE,
            _ => self.name,
        }
    }

    /// MCP `Tool.annotations` (`SPEC.md` §2.3–§2.4, ruling R5).
    pub fn annotations(&self) -> ToolAnnotations {
        match self.name {
            "search" | "ask" => ToolAnnotations {
                read_only_hint: true,
                destructive_hint: false,
                idempotent_hint: false,
                open_world_hint: true,
            },
            _ => ToolAnnotations {
                read_only_hint: false,
                destructive_hint: false,
                idempotent_hint: false,
                open_world_hint: true,
            },
        }
    }

    /// Compact `2024-11-05` protocol wire representation (`name`, `description`, `inputSchema`).
    pub fn wire_2024_11_05(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
        })
    }
}

/// Server instructions string (`SPEC.md` §2.2, <= 450 bytes).
pub fn instructions() -> &'static str {
    SERVER_INSTRUCTIONS
}

fn search_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "Keywords, DOI/arXiv/CVE, FTS5 query (cache), or terms (catalog)."},
            "scope": {"type": "string", "default": "web", "description": "Vertical scope (see description), sub-scope, 'cache', or 'catalog'."},
            "sources": {"type": "array", "items": {"type": "string"}, "description": "Explicit catalog source IDs; bypasses routing."},
            "freshness": {"type": "string", "enum": ["day", "week", "month", "year"]},
            "domains": {"type": "array", "items": {"type": "string"}, "description": "'site.com' includes, '-site.com' excludes."},
            "lang": {"type": "string", "description": "BCP-47 locale hint (auto if omitted)."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10},
            "max_age_hours": {"type": "number", "minimum": 0, "description": "Max cache age (h); 0 forces live."}
        },
        "required": ["query"],
        "additionalProperties": false
    })
}

fn exec_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "code": {"type": "string", "description": "JS async arrow, statements+return, or expression."},
            "max_tokens": {"type": "integer", "minimum": 500, "maximum": 20000, "default": 6000, "description": "Output cap (CJK-aware); overflow -> cache exec:<id>."},
            "timeout_s": {"type": "integer", "minimum": 1, "maximum": 120, "default": 30, "description": "Wall-clock timeout (s); >55s may clip."}
        },
        "required": ["code"],
        "additionalProperties": false
    })
}

fn ask_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "question": {"type": "string", "description": "Factual question."},
            "freshness": {"type": "string", "enum": ["day", "week", "month", "year"]},
            "lang": {"type": "string", "description": "BCP-47 locale hint."},
            "domains": {"type": "array", "items": {"type": "string"}, "description": "'site.com' / '-site.com'."},
            "mode": {"type": "string", "enum": ["auto", "fast", "agent"], "default": "auto", "description": "'fast' (no LLM), 'agent' (LLM), or 'auto'."},
            "max_tokens": {"type": "integer", "minimum": 200, "maximum": 8000, "default": 1500}
        },
        "required": ["question"],
        "additionalProperties": false
    })
}

fn exec_description() -> String {
    let mut out = String::with_capacity(3800);
    out.push_str(EXEC_DESCRIPTION_PREFIX);
    out.push_str(&render_dts_namespaces(true));
    out.push_str("```");
    out
}

/// Exactly `[search, exec, ask]`, generated from [`crate::HOST_API`] (ruling R5).
pub fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "search",
            description: SEARCH_DESCRIPTION.to_owned(),
            input_schema: search_input_schema(),
        },
        ToolDef {
            name: "exec",
            description: exec_description(),
            input_schema: exec_input_schema(),
        },
        ToolDef {
            name: "ask",
            description: ASK_DESCRIPTION.to_owned(),
            input_schema: ask_input_schema(),
        },
    ]
}

/// The `2024-11-05` `result.tools` JSON array (`name`, `description`, `inputSchema` for both tools).
pub fn tools_wire_2024_11_05() -> Value {
    Value::Array(
        tool_defs()
            .into_iter()
            .map(|t| t.wire_2024_11_05())
            .collect(),
    )
}
