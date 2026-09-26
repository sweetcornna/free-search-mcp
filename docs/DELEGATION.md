# Delegating a lookup

An agent can call the tools itself, and for most work that is the right shape.
Delegation is for a narrower case: you want a short answer with sources, and
you want the page text kept out of the caller's context. None of it is on by
default. Every route uses one prompt (`search_mcp.agent.HOST_AGENT_PROMPT`), so
the rules about dates, sources and untrusted page text hold whichever you pick.
Search needs no key on any of them.

| You want | Use | It needs |
|---|---|---|
| The agent to search and read by itself | the tools, as installed | nothing |
| Your host's own subagents to do the lookup | the `quick-search` agent definition | a host with subagents |
| The server to dispatch through Claude Code | `SEARCH_MCP_AGENT_BACKEND=claude-code` | the `claude` CLI, logged in |
| The server to dispatch through Codex | `SEARCH_MCP_AGENT_BACKEND=codex` | the `codex` CLI, logged in |
| The server to call a model endpoint you choose | `SEARCH_MCP_AGENT_BACKEND=api` | an OpenAI-compatible or Anthropic-compatible URL |
| Your own code to supply the model | `ask(question, answer_with=...)` in Python | nothing else |

## With the host's own subagents

In Claude Code the plugin installs `free-search:quick-search`. For a server you
registered by hand, write the agent file yourself:

```bash
uvx free-search-mcp agent-file claude-code > ~/.claude/agents/quick-search.md
```

`--tool-prefix` sets how the host names the server's tools. The default,
`mcp__search__`, matches `claude mcp add search ...`.

For Codex, the same command writes a custom-agent file in the format Codex
documents (`name`, `description`, `developer_instructions`, a read-only sandbox
and low reasoning effort):

```bash
mkdir -p ~/.codex/agents
uvx free-search-mcp agent-file codex > ~/.codex/agents/quick_search.toml
```

One caveat, measured with Codex CLI 0.154 on 2026-09-21: `codex exec` offered
its `spawn_agent` tool without a parameter for choosing an agent by name, so
the file could not be selected from there. Handing the same instructions to a
generic subagent did work in that version. The subagent called the search tools
and returned a sourced answer in 87 s. The `quick_search` MCP prompt returns
that text plus your question for any MCP client, and
`uvx free-search-mcp agent-file prompt` prints it for a platform that has a
system-prompt field and no agent files.

## With the `ask` tool

Set a backend and the server registers a twelfth tool,
`ask(question, freshness, include_domains, category)`. The server first runs
one `research` call, with no model involved, and puts the pages in the model's
first message, so most questions take one model call. The model may then spend
`SEARCH_MCP_AGENT_MAX_STEPS` calls on `search`, `fetch` or `read_doc`, and has
no other tool. When the model fails or the deadline passes, `ask` returns the
pages as a `research` brief together with the reason, so the search is not
wasted. A bad configuration is reported as an error that names the setting.

| Variable | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_AGENT_BACKEND` | `off` | `api`, `claude-code` or `codex` |
| `SEARCH_MCP_AGENT_MODEL` | empty | Required for `api`. Empty means `haiku` on Claude Code and the CLI's own default on Codex |
| `SEARCH_MCP_AGENT_API_PROTOCOL` | `openai` | `openai` (`/chat/completions`) or `anthropic` (`/v1/messages`) |
| `SEARCH_MCP_AGENT_API_BASE_URL` | empty | For `openai`, the URL ending in `/v1`. A local Ollama is `http://localhost:11434/v1` |
| `SEARCH_MCP_AGENT_API_KEY` | empty | The credential of that model endpoint, when it has one. A local endpoint has none |
| `SEARCH_MCP_AGENT_COMMAND` | `claude` / `codex` | Full path of the CLI. Hosts often start MCP servers with a short `PATH` |
| `SEARCH_MCP_AGENT_CLI_ARGS` | backend default | Extra CLI flags, replacing the default. For Codex the default is `--ignore-user-config -c model_reasoning_effort="low"`. If your model provider lives in `config.toml`, keep `--ignore-user-config` and name the provider with `-c` overrides: without that flag the child also loads your own Codex plugins and MCP servers |
| `SEARCH_MCP_AGENT_DEPTH` | `3` | Pages the server reads before the model is called |
| `SEARCH_MCP_AGENT_READ_SECONDS` | `8` | How long those reads may take together. A page still loading after that is left out and its search snippet is used |
| `SEARCH_MCP_AGENT_MAX_STEPS` | `1` | Tool rounds the model may spend afterwards. `0` answers from the first pages only |
| `SEARCH_MCP_AGENT_MAX_SOURCE_CHARS` | `6000` | Characters of each page given to the model |
| `SEARCH_MCP_AGENT_TIMEOUT_SECONDS` | `90` | Deadline for the model part |

The CLI backends start the child with its built-in tools off and with a
three-tool copy of this server. That copy enforces the step limit itself, has
local file reads switched off, and never sees the model endpoint's key. Claude Code runs with extended thinking off, which took the same answer
from 13.7 s to 5.2 s. Codex runs in a read-only sandbox, in an empty directory,
with its own web search disabled so that the answer rests on the pages the
server reports.

Measured on the development machine on 2026-09-21, for the model part only.
The search part took 8 to 9 s cold and nothing when cached. One slow site used
to stretch it: a Baidu Baike page took 25.7 s to fail while the two pages that
held the answer arrived in 0.5 s and 2.1 s, which is why the reads now share a
time budget.

| Backend | First pages were enough | One follow-up call |
|---|---|---|
| `api`, local Ollama `qwen3:1.7b`, no key | 6.4 s | not measured |
| `claude-code`, `haiku` | 4.8 s | 10 to 12 s |
| `codex`, default model, low effort | 13 s | 21 to 24 s |

A small model reads a few pages quickly and can misread them. The tool's own
description tells the calling agent to read the primary page itself for
anything a person will act on.

## Embedding in a service

- Any MCP client can connect over stdio, or over HTTP with
  `--transport streamable-http`.
- `SEARCH_MCP_TOOLS=search,fetch` registers only the named tools, which keeps
  the tool schemas in your model's context to the ones you use.
- `search-mcp ask "question" --json` is the one-shot form for a service that
  would rather start a process. It exits 0 with an answer, 1 when the model
  failed and the pages were printed instead, and 2 when no backend is configured.
- In Python, pass your own model and no setting is needed:

```python
from search_mcp.agent import ask

async def my_model(instructions: str, evidence: str) -> str:
    return await my_llm.complete(system=instructions, user=evidence)

result = await ask("When does registration close?", answer_with=my_model)
print(result["answer"], result["sources"])
```
