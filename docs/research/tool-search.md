# Tool search in MCP gateways and clients

As of: 2026-11-09. Goal: How do other gateways and clients solve “dynamic tool discovery” instead of loading all tools into the context? Derived from that: a concrete recommendation for `search_tools` / `describe_tools` / `call_tool` in emcipi without heavy dependencies.

Where possible, sources are source code at a fixed commit (permalinks) or official documentation. Statements without a primary source are marked **unverified**.

Commits used:

| Repo | Commit |
|---|---|
| earendil-works/pi | `7d073328e44337d3292a7db9b0f0308f679c5e97` |
| openai/codex | `8703d32bd0ea5cf26768859a3dd476df906ba053` |
| UsefulSoftwareCo/executor (branch `v2`) | `2f4c398f3906e0570f0e25a16fa3602329baba29` |
| 1mcp-app/agent | `c51c5228d1bd599ee8f3e443f2d2e7e50381b5a1` |
| docker/mcp-gateway | `7e0f34277c91855ad861eeff536b4fdc77420593` |
| stacklok/toolhive | `42cd9b7e497bfc77dc67a1e73037675247d405b1` |
| cloudflare/agents | `5c33d89791d6a00c6ea64cf7790fa753e3e5cbfd` |
| microsoft/vscode-copilot-chat | `5863f5a7088958050792b5dccbe8b46c6e13eccc` |
| metatool-ai/metamcp | `250240be7da37e3f21d53eaa66afac5eee775aa5` |

## Summary

- **BM25 has prevailed.** Anthropic (BM25 variant), Pi, Codex, Executor, Docker (`mcp-find`) and ToolHive (FTS5) rank lexically with Okapi BM25, almost all with `k1 = 1.2`, `b = 0.75` (Docker: `k1 = 1.5`). Embeddings are only optional (Docker `find-tools`, ToolHive hybrid, VS Code).
- **The name counts more than the description.** Pi weights a name match with 3×IDF so that it beats any number of description matches, and sorts first by coverage of the query terms. Docker weights via token repetition (server name ×4, tool name ×2), Cloudflare via field weights.
- **Indexed are name, description, parameter names and parameter descriptions**, usually also the server or namespace name and description. This applies to Anthropic (officially documented), Pi, Codex and Executor.
- **Tokenization:** camelCase and snake_case are split, there is a naive plural stemmer and (in Pi) a short English stop-word list. Pi and Executor use nearly identical code.
- **Result size:** default 5 (Anthropic, Claude Code), 8 (Pi, Codex, ToolHive), 10 (Docker, Executor), 20 (1MCP).
- **Two result models:** (a) The found tools are **declared** for the next model call (Anthropic `tool_reference`, Claude Code, Pi `tool_search`, Codex, VS Code, Docker `mcp-add`). (b) The search returns names and a short summary, and invocation goes through a **proxy tool** (1MCP `tool_invoke`, Executor `call`, ToolHive `call_tool`, Docker `mcp-exec`, emcipi `call_tool`). Variant (a) is implemented almost only by clients or the API itself, because only they can extend the tool list in a cache-friendly way. On the server side (gateway), (b) dominates.

## Comparison table

| System | Ranking | Searched fields | Default / Max | Schema in search result | Afterwards |
|---|---|---|---|---|---|
| Anthropic API, `tool_search_tool_regex_20251119` | Python regex (`re.search`, case-insensitive), ≤200 characters | Name, description, argument names, argument descriptions | 5 / 10 000 | `tool_reference`, the API expands it to the full definition | Deferred loading, inline in the history, cache is preserved |
| Anthropic API, `tool_search_tool_bm25_20251119` | BM25, natural language, ≤500 characters | as above | 5 / 10 000 | as above | as above |
| Claude Code `ToolSearch` | not public (**unverified**) | Name, description (up to 4 096 characters) | 5 (observed) | full JSON schemas | Tool directly callable afterwards |
| Pi `tool_search` | fielded BM25 (k1 1.2, b 0.75, name 3×IDF, namespace 0.5×IDF, coverage first) | Name, description, property names and descriptions recursively, namespace name/description/instructions | 8 / – | no (name and first line) | `setActiveTools`, declared from the next call on |
| Pi codemode `searchTools()` | same ranker | same | 8 / – | no, `describeTool()` returns the TS declaration | Call in the script `tools.<name>()` |
| OpenAI Codex `tool_search` | BM25 (crate `bm25` 2.3.2, English tokenizer) | Name, name with `_`→space, server, title, description, connector, namespace description, top-level property names | 8 / – | yes, loadable specs (`defer_loading: true`) | loaded for the next model call |
| Executor `search` | BM25 in Postgres (k1 1.2, b 0.75), one field | Path/name, title, description, top-level input names and descriptions, router/app titles and descriptions, account labels | 10 / page budget 25 kB | one-line input types (≤1 000 characters); full signature via `describe` | Proxy: `call` (also batch) or `execute` (JS) |
| 1MCP lazy loading `tool_list` | no ranking; glob (`*`, `?`) on the tool name, filter by server/tag | Tool name | 20 / 5 000 | no, `tool_schema` separately | Proxy: `tool_invoke` |
| Docker MCP Gateway `mcp-find` | BM25 (k1 1.5, b 0.75), field weight via token repetition; alternatively embeddings | **Server**: name ×4, title ×3, tool names ×2, descriptions ×1 | 10 / – | Config schema of the server | `mcp-add` registers the tools (→ `tools/list_changed`), alternatively `mcp-exec` |
| Docker `find-tools` (optional) | Embeddings (`text-embedding-3-small`, OpenAI) | Tool embeddings | 5 fixed | yes (`inputSchema`) | – |
| ToolHive vMCP Optimizer `find_tool` | hybrid: SQLite FTS5 BM25 (Porter) + embedding cosine, ratio 0.5 | Name, description | 8 / – | yes, full `mcp.Tool` | Proxy: `call_tool` |
| Cloudflare Code Mode `codemode.search()` | weighted field matches with exact/prefix boni and minimum coverage | Path, connector, method, description | 50 | no, `describe()` returns TS docs | Call in code |
| VS Code Copilot “virtual tools” | LLM grouping plus 10 embedding matches | Group summaries | from 64 tools | – | `activate_*` expands a group into the tool list |
| MetaMCP | no search (roadmap) | – | – | – | – |
| emcipi (current) | Substring: +3 name, +1 description/title | qualified name, description, title | 10 / 50 | optional (`include_schema`) | Proxy: `call_tool` |

---

## 1. Anthropic Tool Search Tool (Claude API)

Source: https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-search-tool

**Variants**

- `tool_search_tool_regex_20251119` (name `tool_search_tool_regex`): “Claude writes Python `re.search()` patterns, not natural language queries. Matching is case-insensitive.” At most 200 characters per pattern.
- `tool_search_tool_bm25_20251119` (name `tool_search_tool_bm25`): “Claude searches with natural language queries. Maximum query length: 500 characters.”
- Searched fields (verbatim): “Both tool search variants (`regex` and `bm25`) search tool names, descriptions, argument names, and argument descriptions.”

**Flow and `defer_loading`**

- All tools are sent with their full definition in the request. Tools with `defer_loading: true` do not end up in the context. At least one tool must be non-deferred, otherwise the result is HTTP 400 with “At least one tool must have defer_loading=false.”
- The search returns `tool_reference` blocks, which the API automatically expands into full definitions: “returns the matching tools as `tool_reference` blocks (up to 5 by default, although Claude can set a `limit` in its search input)”.
- `limit`: “an integer from 1 to 10,000 that caps how many matching tools the search returns (default: 5)”. Up to 10 000 deferred tools per request.
- Result format: `tool_search_tool_result` → `tool_search_tool_search_result.tool_references[] = {type: "tool_reference", tool_name}`. Without matches, an empty array is returned and no error.
- A custom search is possible: a client tool returns `tool_result` with `[{ "type": "tool_reference", "tool_name": "…" }]`, for example for embedding search.
- Prompt caching: “Deferred tools are not included in the system-prompt prefix. When the model discovers a deferred tool through tool search, the definition is appended inline as a `tool_reference` block in the conversation history. The prefix is untouched, so prompt caching is preserved.” By contrast: “Modifying tool definitions” invalidates the “Entire cache (tools, system, messages)”. Source: https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-use-with-prompt-caching

**Recommendations from the docs** (relevant for emcipi):

- “Keep your 3–5 most frequently used tools non-deferred.”
- “Use consistent namespacing in tool names: prefix by service or resource (for example, `github_`, `slack_`) so one search matches the whole group.”
- “Add a system prompt section describing available tool categories: ‘You can search for tools to interact with Slack, GitHub, and Jira.’”
- Worthwhile from “10 or more tools” or when tool definitions take up “more than 10k tokens”.

**Key figures** (Anthropic Engineering, “Advanced tool use”, 2025-11-24, https://www.anthropic.com/engineering/advanced-tool-use): “58 tools consuming approximately 55K tokens”. With tool search “Opus 4 improved from 49% to 74%, and Opus 4.5 improved from 79.5% to 88.1%” in MCP evals with large tool libraries.

## 2. Claude Code MCP Tool Search

Source: https://code.claude.com/docs/en/mcp (sections “Scale with MCP tool search”, “Configure tool search”)

- On by default: “Tool search is enabled by default: MCP tools are deferred and discovered on demand.” And: “Only tool names and server instructions load at session start”.
- `ENABLE_TOOL_SEARCH`:
  - (unset) / `true`: all MCP tools deferred
  - `auto`: “Tools that would otherwise be deferred load upfront while their definitions total under 10% of the context window. At 10% or more, tool search turns on”
  - `auto:N`: custom percentage, e.g. `auto:5`
  - `false`: everything upfront
- Exceptions: `alwaysLoad: true` per server in the config, `_meta["anthropic/alwaysLoad"]` per tool (set by the server author).
- For server authors: “the server instructions field becomes more useful with tool search enabled. Server instructions help Claude understand when to search for your tools”. The instructions should explain: “What category of tasks your tools handle / When Claude should search for your tools / Key capabilities your server provides”.
- Description lengths: 4 096 characters for instructions and upfront-loaded tools, 16 384 for tools loaded via search. “When tool search matches Claude's query against a tool's description, it reads only the part within the shorter limit, so put the words Claude would search for near the start too.”
- `list_changed`: “In an interactive terminal session, Claude Code fetches the updated list from that server”. In `-p` mode and in the Agent SDK it “refreshes only the tool list on these notifications”.

**The meta tool itself** is not officially documented. The following definition comes from a running Claude Code / Agent SDK session (as of 2026-11-09) and is only **observed**, not officially confirmed:

> “Fetches full schema definitions for deferred tools so they can be called. Deferred tools appear by name in <system-reminder> messages. Until fetched, only the name is known — there is no parameter schema, so the tool cannot be invoked. This tool takes a query, matches it against the deferred tool list, and returns the matched tools' complete JSONSchema definitions inside a <functions> block. […] Query forms: `"select:Read,Edit,Grep"` — fetch these exact tools by name; `"notebook jupyter"` — keyword search, up to max_results best matches; `"+slack send"` — require "slack" in the name, rank by remaining terms”

Parameters: `query` and `max_results` (default 5). Claude Code does not disclose the ranking algorithm (**unverified**). Of interest for emcipi are the three query forms: exact selection, keyword search and a required term in the name.

**Consequence for emcipi:** Claude Code defers MCP tools anyway. If Claude Code is attached to `/mcp/dynamic`, the model searches in two stages (Claude Code's `ToolSearch` loads `search_tools`, then `search_tools` searches). If it is attached to `/mcp/static`, Claude Code does the search itself, but then costs one name line per tool in the context. With very many tools, there are still good arguments for `dynamic`. This is a trade-off, not a bug.

## 3. Pi (earendil-works/pi, packages/coding-agent)

Docs: [mcp.md](https://github.com/earendil-works/pi/blob/7d073328e44337d3292a7db9b0f0308f679c5e97/packages/coding-agent/docs/mcp.md), [codemode.md](https://github.com/earendil-works/pi/blob/7d073328e44337d3292a7db9b0f0308f679c5e97/packages/coding-agent/docs/codemode.md), [cli.md](https://github.com/earendil-works/pi/blob/7d073328e44337d3292a7db9b0f0308f679c5e97/packages/coding-agent/docs/cli.md)

### Exposure modes (mcp.md, “Control tool exposure”)

| Exposure | Behavior according to the docs |
|---|---|
| `codemode` (default) | “Callable from `codemode` scripts, but neither declared to the model nor listed in the codemode description. Scripts find tools with `searchTools()`, `describeTool()`, or `ALL_TOOLS`.” |
| `deferred` | “Not declared until `tool_search` loads a match for the next model call.” |
| `direct` | “Declared to the model like a built-in tool and also callable from codemode.” |
| `hidden` | “Registered but unreachable.” |

`toolExposure` overrides per tool, also with `*` patterns. Servers with `codemode`/`deferred` end up in an `mcp_servers` section of the system prompt with one line from `description` or the first line of the server instructions. The section is limited to 250 characters per server and 4 096 characters in total ([mcp/index.ts L159–L164](https://github.com/earendil-works/pi/blob/7d073328e44337d3292a7db9b0f0308f679c5e97/packages/coding-agent/src/extensions/mcp/index.ts#L159-L164)). If it changes, Pi appends it to the history “instead of changing tool declarations, so earlier messages stay cached” (mcp.md). The introductory text ([L167–L172](https://github.com/earendil-works/pi/blob/7d073328e44337d3292a7db9b0f0308f679c5e97/packages/coding-agent/src/extensions/mcp/index.ts#L167-L172)):

> “MCP servers whose tools are not declared to you. Call the tools of `codemode` servers from codemode scripts. Load the tools of `tool_search` servers with `tool_search`.”

### Ranking (source code)

File: [`src/extensions/tool-search/tool.ts`](https://github.com/earendil-works/pi/blob/7d073328e44337d3292a7db9b0f0308f679c5e97/packages/coding-agent/src/extensions/tool-search/tool.ts). `tool_search` and codemode `searchTools()` use the same ranker ([execute.ts L669–L697](https://github.com/earendil-works/pi/blob/7d073328e44337d3292a7db9b0f0308f679c5e97/packages/coding-agent/src/extensions/codemode/execute.ts#L669-L697)).

- **Tokenizer** (L78–L93): NFC, camelCase split via `([a-z0-9])([A-Z])` → `$1 $2` and `([A-Z]+)([A-Z][a-z])` → `$1 $2`, lowercase, everything except letters/digits becomes a space, `Intl.Segmenter` for non-ASCII. Then stop words are removed and stemming is applied.
- **Stop words** (L45–L67): `a an and are as at be by for from in is it of on or that the this to with`.
- **Stemmer** (L70–L75): “Naive singular form”, `ies`→`y` (length >4), `ches|shes|sses|xes|zes` → `-es` (length >4), otherwise strip `s` (length >3, not `ss`).
- **Document** (L104–L141): `text` = description plus recursively all schema `description`s and property names (follows local `$ref`s, `items`, `anyOf/oneOf/allOf`). The name is a separate field. `namespaceText` = server name + description + instructions.
- **Score** (L191–L261):
  - IDF: `ln(1 + (N − df + 0.5) / (df + 0.5))`, df counts matches in name, text or namespace
  - per query term: `idf × (3·[term in name] + 0.5·[term in namespace] + tf·(k1+1) / (tf + k1·(1 − b + b·len/avglen)))`, `k1 = 1.2`, `b = 0.75`. Only the description is length-normalized.
  - Comment on `NAME_WEIGHT = 3`: “The text part of a term is at most `k1 + 1` (2.2) times its IDF and about 1 for a single mention, so a name match beats any number of mentions in the description.”
  - `NAMESPACE_WEIGHT = 0.5`: “They are shared by every tool of the server, so they help find the server but not the tool in it.”
  - Bonus: If all terms of the “local” name (last `__` segment) occur in the query, the sum of all IDFs is added.
  - Sorting: first **exact name** (whole query = name or `__` suffix), then **coverage** (number of matched query terms), then score. From the comment: “for `screenshot window`, a tool whose description has both words beats `window`, whose name has one”.
- **Default limit** 8 (L22), no maximum.

### `tool_search` (meta tool)

Description (L320), verbatim:

> “# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nSome of the tools, such as tools of MCP servers, may not have been provided to you upfront, and you should use this tool (`tool_search`) to search for the required tools. For MCP tool discovery, always use `tool_search`.”

Parameters (L264–L269): `query` (“Search query for deferred tools.”), `limit` (“Maximum number of tools to return. Defaults to 8.”). The description is intentionally static: “It does not list the searchable tools or their namespaces, so it stays the same while tools are registered” (L316–L319).

Result (L340–L345): `Loaded N tools. They are available from your next call:` followed by `- name: first line of the description`. Matches are activated via `setActiveTools([...active, ...matches])` (L300–L314), are recorded in the transcript and survive `/tree`, resume and fork.

### codemode

Globals according to codemode.md: `searchTools(query, { limit?, namespace? })` “Rank callable tools by relevance (BM25, default limit 8). Resolves to `{ name, description }[]`.”, `describeTool(name)` “Resolves to a tool's description and TypeScript declaration”, `describeNamespace(name)`, `ALL_TOOLS`. The codemode description lists declared tools with TS declarations up to a budget of 3 000 estimated tokens. Deferred tools do not appear, so that the description stays stable. Hint line in the tool description ([codemode/tool.ts L133](https://github.com/earendil-works/pi/blob/7d073328e44337d3292a7db9b0f0308f679c5e97/packages/coding-agent/src/extensions/codemode/tool.ts#L128-L139)):

> “- `ALL_TOOLS`, `await searchTools(query, { limit?, namespace? })`, `await describeTool(name)`, `await describeNamespace(name)`: find unlisted tools, such as MCP tools.”

## 4. OpenAI Codex (`tool_search`, Rust)

Codex is a Rust client using `rmcp` and is therefore particularly close to emcipi.

- Ranking: crate [`bm25`](https://crates.io/crates/bm25) version 2.3.2 ([Cargo.toml L367](https://github.com/openai/codex/blob/8703d32bd0ea5cf26768859a3dd476df906ba053/codex-rs/Cargo.toml#L367)), `EmbedderBuilder::with_fit_to_corpus(Language::English, &corpus)` and `Scorer::upsert`/`matches` ([tool_search.rs L166–L175, L334–L347](https://github.com/openai/codex/blob/8703d32bd0ea5cf26768859a3dd476df906ba053/codex-rs/core/src/tools/handlers/tool_search.rs#L166-L175)). There is a single text field, no field weights. With the default feature `default_tokenizer`, the crate pulls in the dependencies `rust-stemmers`, `stop-words`, `deunicode`, `unicode-segmentation` and `cached` (crates.io metadata 2.3.2).
- Search text for MCP tools ([mcp.rs L603–L650](https://github.com/openai/codex/blob/8703d32bd0ea5cf26768859a3dd476df906ba053/codex-rs/core/src/tools/handlers/mcp.rs#L603-L650)): flat tool name, callable name, original tool name, server name, title, description, connector name, namespace description, plugin names, sorted top-level property names. For non-MCP functions, additionally `name.replace('_', " ")` and recursively the schema descriptions ([tools/src/tool_search.rs L171–L196](https://github.com/openai/codex/blob/8703d32bd0ea5cf26768859a3dd476df906ba053/codex-rs/tools/src/tool_search.rs#L171-L196)).
- `TOOL_SEARCH_DEFAULT_LIMIT = 8` ([tool_discovery.rs L7](https://github.com/openai/codex/blob/8703d32bd0ea5cf26768859a3dd476df906ba053/codex-rs/tools/src/tool_discovery.rs#L7)).
- Deferral: If search is active, all MCP tools become `ToolExposure::Deferred` ([mcp_tool_exposure.rs L158–L162](https://github.com/openai/codex/blob/8703d32bd0ea5cf26768859a3dd476df906ba053/codex-rs/core/src/mcp_tool_exposure.rs#L158-L162)). Matches are returned as loadable namespace specs with `defer_loading = Some(true)` and are loaded for the next model call ([tool_search.rs L98–L132](https://github.com/openai/codex/blob/8703d32bd0ea5cf26768859a3dd476df906ba053/codex-rs/tools/src/tool_search.rs#L98-L132)).
- Description ([tool_search_spec.rs L93–L96](https://github.com/openai/codex/blob/8703d32bd0ea5cf26768859a3dd476df906ba053/codex-rs/core/src/tools/handlers/tool_search_spec.rs#L93-L96)):

> “# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nYou have access to tools from the following sources:\n- <source>: <description>\n…\nSome of the tools may not have been provided to you upfront, and you should use this tool (`tool_search`) to search for the required tools. For MCP tool discovery, always use `tool_search` instead of `list_mcp_resources` or `list_mcp_resource_templates`.”

  Parameters: `query` (“Search query for deferred tools.”), `limit` (“Maximum number of tools to return. Defaults to 8.”). Pi's text is recognizably derived from it. Pi itself refers to Codex in its code: “as Codex allows for deferred namespaces” (mcp/index.ts L158).

## 5. Executor (UsefulSoftwareCo/executor)

Executor is a gateway with a single MCP endpoint for many “apps” (OpenAPI, MCP, GraphQL). Docs: [apps/docs/content/mcp.md](https://github.com/UsefulSoftwareCo/executor/blob/2f4c398f3906e0570f0e25a16fa3602329baba29/apps/docs/content/mcp.md). “The endpoint does not expose one MCP tool per app tool. However many apps you add, the tool list stays the same size.” The tools are `skills`, `search`, `describe`, `call`, `execute` and `resume`. Modes exist for elicitation (model/browser/native), not for tool exposure.

**Ranking** ([packages/sdk/src/implementation/tool-search.ts](https://github.com/UsefulSoftwareCo/executor/blob/2f4c398f3906e0570f0e25a16fa3602329baba29/packages/sdk/src/implementation/tool-search.ts), [tool-search-store.ts](https://github.com/UsefulSoftwareCo/executor/blob/2f4c398f3906e0570f0e25a16fa3602329baba29/packages/sdk/src/implementation/tool-search-store.ts)):

- `words()` (L24–L38): camelCase split (same regexes as Pi), lowercase, split on non-letters/digits, stemmer `ies`→`y`, strip `s`. No stop words.
- Fields (L66–L82): root router title and description, `tool.name`, `tool.title`, `tool.description`, top-level input property names and descriptions (L44–L60), router title and description, plus the labels of the target or account. Everything goes into one bag of words, without field weights.
- Okapi BM25 in SQL, `const BM25 = { k1: 1.2, b: 0.75 }` (store L98, formula L359–L363). Without query terms, results are sorted by target and name.

**Limits** ([packages/mcp/src/contracts/execute.ts L359–L364, L496](https://github.com/UsefulSoftwareCo/executor/blob/2f4c398f3906e0570f0e25a16fa3602329baba29/packages/mcp/src/contracts/execute.ts#L359-L364)): `defaultItems: 10`, `descriptionChars: 200` (first line), `inputChars: 1_000` (one-line input type, otherwise `inputTruncated`), `describePaths: 20`, page budget `searchPageBytes = 25_000`. A match contains `path`, `description` (first line), `input` (type on one line). App and accounts are listed once per result as `namespaces`, not in every item. Paging via `next`.

**Unknown paths:** `describe` and `call` return “the closest paths” via Levenshtein ([packages/mcp/src/implementation/suggestions.ts](https://github.com/UsefulSoftwareCo/executor/blob/2f4c398f3906e0570f0e25a16fa3602329baba29/packages/mcp/src/implementation/suggestions.ts)).

**Meta tool descriptions** (execute.ts L367–L385, L524–L548), verbatim:

- `search`: “Find Executor app tools by words in their paths, descriptions and labels. Returns exact paths, one-line descriptions and input types, with each app and its linked accounts listed once. Pass a path to call or describe, or use it as tools.<path> inside execute. Omit query to list a namespace. To read the following page, call search again with the returned next object as the whole input; next is null on the last page.”
  - `query`: “Words matched against tool paths, descriptions, input names, and app, account and router labels. Omit it to list a namespace.”
  - `namespace`: “App slug, such as axiom, or a namespace inside it, such as acme.issues.”
  - `limit`: “Most items to return, 10 by default. A page ends sooner when it reaches its size budget.”
- `describe`: “Read the whole description and TypeScript signature, with input and output types, of tools at exact paths from search. Unknown paths are listed in missing with the closest paths.”
- `call`: “Call Executor app tools directly by exact path from search. Pass one {path, input} as calls, or an array of independent calls to run them concurrently; results come back in the same order, each with ok and its value or error. Use execute instead when one call's output feeds another, or to filter a large result before it reaches you. […]”
- `execute`: “Run a JavaScript program over Executor apps, for work where one result feeds another call or a large result should be filtered before it reaches you; for one or a few independent calls, use call instead. Find tools with the search tool or tools.search inside the program, and full signatures with describe or tools.search.describe({ paths }). […]”

## 6. 1MCP (1mcp-app/agent), “OneMCP”

There are several projects under the name “OneMCP”. The aggregator gateway with several exposure modes is **1MCP** (`1mcp-app/agent`). `Gentoro-OneMCP/onemcp`, by contrast, is an LLM-backed natural-language API runtime (“exposes your system through a single, natural-language interface”, [README](https://github.com/Gentoro-OneMCP/onemcp)) and is not relevant for tool search.

1MCP has three modes:

1. **Direct** (default): all tools passed through.
2. **Lazy Loading** (`1mcp serve --enable-lazy-loading`): “Opt into meta-tool exposure for progressive tool discovery” ([docs serve.md](https://github.com/1mcp-app/agent/blob/c51c5228d1bd599ee8f3e443f2d2e7e50381b5a1/docs/site/en/commands/serve.md)). README L162: “It keeps the backend discovery and invocation surface at `tool_list`, `tool_schema`, and `tool_invoke`”.
3. **CLI Mode**: The agent uses the shell commands `1mcp instructions`, `1mcp inspect <server>`, `1mcp inspect <server>/<tool>` and `1mcp run <server>/<tool> --args '<json>'` (“This is progressive disclosure for tools.”, [cli-mode.md](https://github.com/1mcp-app/agent/blob/c51c5228d1bd599ee8f3e443f2d2e7e50381b5a1/docs/site/en/guide/integrations/cli-mode.md)).

**Search:** There is no ranking. `tool_list` filters by `server`, `tag` and `pattern`. The pattern is a glob on the tool name only (`*`→`.*`, `?`→`.`, anchored `^…$`). Default page size 20, maximum 5 000 ([toolRegistry.ts L209–L274](https://github.com/1mcp-app/agent/blob/c51c5228d1bd599ee8f3e443f2d2e7e50381b5a1/src/core/capabilities/toolRegistry.ts#L209-L274)). The CLI search (`inspect --search`) does substring or glob matching on `server/tool`, with `--include-descriptions` also on the description ([inspectSearch.ts L86–L91, L119–L121](https://github.com/1mcp-app/agent/blob/c51c5228d1bd599ee8f3e443f2d2e7e50381b5a1/src/core/capabilities/inspectSearch.ts#L86-L91)).

Descriptions ([lazyTools.ts](https://github.com/1mcp-app/agent/blob/c51c5228d1bd599ee8f3e443f2d2e7e50381b5a1/src/core/capabilities/internal/lazyTools.ts)), verbatim:

- `tool_list`: “List all available MCP tools with names and descriptions. Use for tool discovery.” (input `server?`, `pattern?`, `tag?`, `limit?`, `cursor?`)
- `tool_schema`: “Get the full schema for a specific tool including input validation rules” (input `server`, `toolName`)
- `tool_invoke`: “Execute any tool on any MCP server with proper argument validation” (input `server`, `toolName`, `args`)

## 7. Docker MCP Gateway (dynamic tools)

Spec: [docs/feature-specs/dynamic_servers.md](https://github.com/docker/mcp-gateway/blob/7e0f34277c91855ad861eeff536b4fdc77420593/docs/feature-specs/dynamic_servers.md). Enabled via `docker mcp feature enable dynamic-tools`. Docker searches **servers in the catalog**, not individual tools.

- `mcp-find` ([dynamic_mcps.go L19–L46](https://github.com/docker/mcp-gateway/blob/7e0f34277c91855ad861eeff536b4fdc77420593/pkg/gateway/dynamic_mcps.go#L19-L46)):
  > “Find MCP servers in the current catalog by name, title, or description.
  > If the user is looking for new capabilities, use this tool to search the MCP catalog for servers that should potentially be enabled.
  > This will not enable the server but will return information about servers that could be enabled.
  > If we find an mcp server, it can be added with the mcp-add tool, and configured with mcp-config-set.”

  `query` (“Search query to find servers by name, title, or description (case-insensitive)”), `limit` (default 10).
- Ranking without embeddings: BM25 ([findmcps.go L76–L196](https://github.com/docker/mcp-gateway/blob/7e0f34277c91855ad861eeff536b4fdc77420593/pkg/gateway/findmcps.go#L76-L196)). One document per server, “BM25F approximation via token repetition”: server name ×4, title ×3, tool names ×2, description and tool descriptions ×1. According to the comment also tags/category ×3, but these are missing in the code. `k1 = 1.5`, `b = 0.75`. The tokenizer only splits on non-`[a-z0-9]`, without camelCase split and without stemming. The index is rebuilt on every query (L200–L207). With an embedding client (feature `use-embeddings` plus `OPENAI_API_KEY`), vector search is used instead ([reload.go L98–L103](https://github.com/docker/mcp-gateway/blob/7e0f34277c91855ad861eeff536b4fdc77420593/pkg/gateway/reload.go#L93-L110)).
- `mcp-add` (`name`, `activate` default `true`): “Add a new MCP server to the session.” With `activate`, the handler calls `updateServerCapabilities` → `mcpServer.AddTool`/`RemoveTools` ([mcpadd.go L241–L261](https://github.com/docker/mcp-gateway/blob/7e0f34277c91855ad861eeff536b4fdc77420593/pkg/gateway/mcpadd.go#L241-L261)). In the Go SDK, both trigger `notifications/tools/list_changed` ([go-sdk server.go L385, L639](https://github.com/modelcontextprotocol/go-sdk/blob/df0f29525bc6c590b6b68d9aae581ec0f21ebb3e/mcp/server.go#L385)). Docker is therefore the only examined **gateway** that loads found tools via `list_changed`.
- `mcp-exec`: “Execute a tool that exists in the current session. This allows calling tools that may not be visible in listTools results.” (`name`, `arguments`)
- `find-tools` (only with embeddings): “Analyze a task description and recommend relevant MCP tools that could help accomplish it. Uses AI to intelligently match your needs to available tools.” Uses `text-embedding-3-small` with a fixed match count of 5 and returns `{tools: [{name, description, inputSchema}]}` in `tools/list` format ([findtools.go L39, L93, L131–L140, L150](https://github.com/docker/mcp-gateway/blob/7e0f34277c91855ad861eeff536b4fdc77420593/pkg/gateway/findtools.go#L146-L160)).
- `code-mode`: creates a new tool `code-mode-<name>` that runs JS over the tools of several servers ([dynamic_mcps.go L154–L182](https://github.com/docker/mcp-gateway/blob/7e0f34277c91855ad861eeff536b4fdc77420593/pkg/gateway/dynamic_mcps.go#L154-L182), codemode.go L133 `AddTool`).

## 8. Other gateways, proxies and clients

### ToolHive vMCP Optimizer (stacklok/toolhive)

The former project `StacklokLabs/mcp-optimizer` is archived. The functionality now lives in `pkg/vmcp/optimizer`. “When the optimizer is enabled, vMCP exposes only two tools to clients: find_tool […] call_tool” ([optimizer.go L1–L12](https://github.com/stacklok/toolhive/blob/42cd9b7e497bfc77dc67a1e73037675247d405b1/pkg/vmcp/optimizer/optimizer.go#L1-L12)).

- Ranking: hybrid. SQLite FTS5 with `tokenize='porter'` on `name` and `description` ([schema.sql](https://github.com/stacklok/toolhive/blob/42cd9b7e497bfc77dc67a1e73037675247d405b1/pkg/vmcp/optimizer/internal/toolstore/schema.sql)) plus optional embedding cosine. Defaults: `DefaultMaxToolsToReturn = 8`, `DefaultHybridSemanticToolsRatio = 0.5`, `DefaultSemanticDistanceThreshold = 1.0` ([sqlite_store.go L34–L46](https://github.com/stacklok/toolhive/blob/42cd9b7e497bfc77dc67a1e73037675247d405b1/pkg/vmcp/optimizer/internal/toolstore/sqlite_store.go#L34-L46)). FTS terms are OR-combined. The keyword source is `tool_keywords`, otherwise the words from `tool_description` (L553–L555).
- Result: full `mcp.Tool` including input schema, plus `token_metrics` (optimizer.go L395–L403).
- Descriptions ([decorator.go L62–L87](https://github.com/stacklok/toolhive/blob/42cd9b7e497bfc77dc67a1e73037675247d405b1/pkg/vmcp/session/optimizerdec/decorator.go#L62-L87)):
  - `find_tool`: “Find and return tools that can help accomplish the user's request. This searches available MCP server tools using semantic and keyword-based matching. Use this function when you need to: (1) discover what tools are available for a specific task, (2) find the right tool(s) before attempting to solve a problem, (3) check if required functionality exists in the current environment. Returns matching tools ranked by relevance including their names, descriptions, required parameters and schemas, plus token efficiency metrics showing baseline_tokens, returned_tokens, and savings_percent. Always call this before call_tool to discover the correct tool name and parameter schema.”
    - `tool_description`: “Description of the task or capability needed (e.g. 'web search', 'analyze CSV file', 'send an email'). This is used for semantic similarity matching against available tools.”
    - `tool_keywords`: “Optional keywords driving the BM25 keyword-search arm (e.g. ['list', 'issues', 'github'] or ['SQL', 'query', 'postgres']). […]”
  - `call_tool`: “Execute a specific tool with the provided parameters. Use this function to run a tool after identifying it with find_tool. Important: always use find_tool first to get the correct tool_name and parameter schema before calling this function.”
  - Robustness: `call_tool` also accepts a `tool_name` that the model mistakenly nested in `parameters` (optimizer.go, `resolveCallToolTarget`).

### VS Code Copilot Chat (“virtual tools”)

[virtualToolsConstants.ts](https://github.com/microsoft/vscode-copilot-chat/blob/5863f5a7088958050792b5dccbe8b46c6e13eccc/src/extension/tools/common/virtualTools/virtualToolsConstants.ts): From `START_GROUPING_AFTER_TOOL_COUNT = HARD_TOOL_LIMIT / 2` (“64, currently”) on, tools are grouped. The groups become virtual tools with the prefix `activate_` and an LLM-generated summary. In addition, there are `NUM_EMBED_MATCHED_TOOLS = 10` matched via embeddings to the user request. Description frame of the group tools ([virtualToolGrouper.ts L29–L30](https://github.com/microsoft/vscode-copilot-chat/blob/5863f5a7088958050792b5dccbe8b46c6e13eccc/src/extension/tools/common/virtualTools/virtualToolGrouper.ts#L29-L30)):

> “Call this tool when you need access to a new category of tools. The category of tools is described as follows:\n\n<summary>\n\nBe sure to call this tool if you need a capability related to the above.”

### MetaMCP (metatool-ai/metamcp)

MetaMCP has no tool search. The README states “Use as Elasticsearch for MCP tool selection (coming soon)” (L85) and the roadmap lists “Dynamically apply search rules on MetaMCP endpoints” (L475) ([README](https://github.com/metatool-ai/metamcp/blob/250240be7da37e3f21d53eaa66afac5eee775aa5/README.md)). What exists is a “filter-inactive-tools” middleware, i.e. static filtering.

### apify/mcpc

`mcpc grep <pattern>` “searches tools and server instructions”, case-insensitive as a substring or with `-E` as a regex, `-m` limits the matches. Explicitly intended as “dynamic tool discovery” ([README](https://github.com/apify/mcpc/blob/6c30b7ca17d82151e1605d8cb22d67d15602132d/README.md)).

### Not found

For `mcpjungle/MCPJungle`, `mcp-router/mcp-router` (archived), `sitbon/magg` and `IBM/mcp-context-forge`, GitHub code search for `search_tools`, `find_tool`, `tool_search` and `semantic_search` found no agent-facing tool search. For context-forge there are only admin/test hits. Code search is incomplete, so this is only weak evidence.

## 9. Code execution approaches (brief)

- **Cloudflare Code Mode.** Originally: “Convert the MCP tools into a TypeScript API, and then ask an LLM to write code that calls that API.” At the time: “Currently, the entire API is loaded” (https://blog.cloudflare.com/code-mode/, 2025-09-26). Later came the Cloudflare MCP server with only `search()` (“Search the Cloudflare OpenAPI spec. All $refs are pre-resolved inline.”) and `execute()` (“Execute JavaScript code against the Cloudflare API.”) for over 2 500 endpoints in “around 1,000 tokens”. The model filters the spec itself via JavaScript (https://blog.cloudflare.com/code-mode-mcp/, 2026-02-20). In the SDK, `codemode.search()` has its own ranking: “Executor-style ranked search” with field weights path 12, method 10, connector 8, description 5. A field exactly equal to the query counts ×14, prefix ×9, phrase ×6. Per token: exact ×4, prefix ×2, substring ×1. Minimum coverage is 100% for ≤2 terms and 60% otherwise, full coverage adds +25. Limit 50 ([connectors/search.ts](https://github.com/cloudflare/agents/blob/5c33d89791d6a00c6ea64cf7790fa753e3e5cbfd/packages/codemode/src/connectors/search.ts#L11-L161)).
- **Anthropic “Code execution with MCP”** (2025-11-04, https://www.anthropic.com/engineering/code-execution-with-mcp): Tools live as a file tree under `./servers/`. An optional `search_tools` has a “detail level parameter” with the levels “name only, name and description, or the full definition with schemas”. The example case shrinks from “150,000 tokens to 2,000 tokens”.
- **Pi codemode** and **Executor `execute`** (see above) combine both: a search as a function in the sandbox script and tools as functions.

For emcipi (a small Rust gateway without a JS sandbox), Code Mode is out of scope. What carries over is the **detail level** in the search result.

---

## Recommendation for emcipi

Prioritized by benefit per effort. Everything can be implemented without new dependencies.

### P1: Fielded BM25 instead of substring score

The model is Pi's `Bm25Ranker`, which is the most carefully reasoned one in this comparison. At around 150 lines of Rust it can be ported directly. The crate `bm25` (Codex) is an alternative, but only supports one field and pulls in five dependencies with the default tokenizer. A custom implementation is smaller and allows field weights.

**Fields per tool:**

| Field | Content | Weight |
|---|---|---|
| `name` | Terms of the qualified name (`server__tool`), plus as a subset `local` = terms of the tool name after the separator | Match: `3 × IDF` (binary, not length-normalized) |
| `text` | Title, full description (not just the first sentence), recursively all `properties` names and `description`s of the input schema (including `items`, `anyOf/oneOf/allOf`, local `$ref` with cycle protection) | Okapi BM25, `k1 = 1.2`, `b = 0.75`, length normalization only here |
| `namespace` | Server name, `description` from the config, upstream server instructions (if present) | Match: `0.5 × IDF` (binary) |

- IDF: `ln(1 + (N − df + 0.5) / (df + 0.5))`, df = number of tools with the term in any field (like Pi). N is the set after the `server` filter.
- Score per tool: `Σ_t idf(t) × (3·inName + 0.5·inNamespace + tf·2.2 / (tf + 1.2·(0.25 + 0.75·len/avglen)))`.
- Bonus: If all `local` terms are contained in the query, `Σ idf` is added (Pi).
- **Sorting:** (1) exact name: the normalized query (lowercase, non-`[a-z0-9_]`→`_`) equals the qualified name or is its suffix after the separator. (2) **Number of covered query terms**, descending. (3) Score. (4) Name as a stable tie-breaker.
- Result: only tools with score > 0. An empty query with `server` still lists all tools of the server (existing behavior, also common in Executor).
- Index: build once per tool list and discard on `list_changed`/reconnect. With a few hundred tools, rebuilding per query is also acceptable (Docker does this).

**Rationale:** The current score counts a match in a long description the same as one in a short one, ignores how rare a term is (IDF), and does not search parameters. Parameter names in particular (`repo`, `issue_number`, `branch`) often match user phrasing more precisely than the description. Anthropic officially searches them, as do Pi, Codex and Executor. Sorting by coverage first prevents a single name match from beating a tool that contains all query words.

### P1: Tokenization

Same function for query and documents:

1. Split camelCase/PascalCase: `([a-z0-9])([A-Z])` → `$1 $2`, then `([A-Z]+)([A-Z][a-z])` → `$1 $2` (so `getHTTPResponse` → `get http response`). In Rust this works without a regex crate using a character loop.
2. Lowercase. Every character that is not `char::is_alphanumeric` becomes a separator. This takes care of snake_case, kebab-case, `.`, `/` and the separator `__`.
3. Remove stop words (Pi's list: `a an and are as at be by for from in is it of on or that the this to with`).
4. Naive singular stemming only for ASCII words: `ies`→`y` (length >4), `ches|shes|sses|xes|zes` → `-es` (length >4), otherwise strip `s` (length >3, not on `ss`). This replaces the current `strip_suffix('s')` with `stem.len() >= 3`, which produces e.g. `status`→`statu`. Pi and Executor have the same problem with `status`. Applied consistently on both sides, this is harmless.
5. The minimum length of 2 is dropped. Single-letter tokens are rare, and stop words take care of the noise.

Optional (P2): a prefix fallback as in Cloudflare. If a query term (≥4 characters) has no exact match, a document term that starts with it counts at half weight (`repo` → `repository`, `config` → `configuration`). This is cheap and catches the most common cases that would otherwise need embeddings.

### P1: Result format

Keep it compact, but such that a call often succeeds without `describe_tools`. Executor and 1MCP do something similar (one-line input type or number of required and optional arguments, respectively).

```
3 of 17 matching tools (github: GitHub: Issues, Pull Requests, Repositories, Code Search)
- github__create_issue(owner, repo, title, body?, labels?): Create a new issue in a GitHub repository.
- github__update_issue(owner, repo, issue_number, state?, title?, …): Update an existing issue.
- github__add_issue_comment(owner, repo, issue_number, body): Add a comment to an issue.

Call `describe_tools` for full input schemas, then `call_tool`.
```

- Signature: top-level property names, optional ones with `?`, truncated after about 120 characters with `…`.
- Description: first line, capped at about 200 characters (Executor `descriptionChars: 200`). The current `first_sentence` remains useful as a fallback.
- Server description once per occurring server instead of per tool (Executor `namespaces`).
- **Default limit 8** (Pi, Codex, ToolHive), maximum 50 stays. Anthropic uses 5, Docker and Executor 10. 8 is a good middle ground because the signature takes a bit more space per line.
- No matches: list server names with their descriptions and point to `server` plus an empty query. For queries that look like a tool name, suggest the 3 closest names via Levenshtein (Executor).
- `describe_tools` and `call_tool` with an unknown name: likewise suggest the closest names instead of just `unknown` (Executor `missing[].matches`). Limit `describe_tools` to about 20 names (Executor `describePaths: 20`).
- `include_schema` stays as a detail level. This corresponds to the “detail level” from Anthropic's code execution article.

### P2: Meta tool descriptions

The texts should name the search mechanism (Pi and Codex explicitly write “with BM25”), give examples of good queries and define the workflow. The server list may stay in the description because it comes from the config and is static. Pi only removes it because its server summaries change on connect. Cap the length per server like Pi (250 characters, 4 096 total). Proposal:

`search_tools`:

> Search the tools of the connected MCP servers. Most tools of this gateway are not listed directly; use this tool to find them. Ranks tools with BM25 over tool names, descriptions and parameter names. Use a few keywords for the action and the object, e.g. "create issue", "list pull requests", "read file". An exact tool name ranks that tool first. Returns tool names with their parameters and a one-line description. An empty query with `server` lists all tools of that server. Servers:
> - github: GitHub: Issues, Pull Requests, Repositories, Code Search
> - …

- `query`: “Keywords for what you want to do, e.g. 'create issue' or 'search code'. Or an exact tool name.”
- `server`: “Only search the tools of this server.”
- `limit`: “Maximum number of results (default 8).”
- `include_schema`: “Return full input schemas instead of one-line signatures.”

`describe_tools`:

> Get the full definitions (description and input schema) of tools by their exact names from search_tools. Unknown names are reported with the closest matching names.

`call_tool`:

> Run a tool found via search_tools. Pass its exact name and arguments matching its input schema; if you are unsure about the arguments, check them with describe_tools first.

Server instructions (`get_info`): as Claude Code recommends, name the categories, not just the mechanism. For example: “This gateway connects GitHub, filesystem, … tools. They are not listed directly: find them with `search_tools`, …” The categories can be generated from the servers' `description`s.

### P3: Variant “load tools” via `tools/list_changed`

Feasible, but **not as the default**:

- **In favor of the approach:** The model calls the tool natively, with the schema in context and client-side validation. Pi (`tool_search` → `setActiveTools`), Codex, Anthropic (`tool_reference`) and Docker (`mcp-add` → `AddTool` → `list_changed`) do it this way.
- **Prompt cache:** Pi, Codex and Anthropic load *client-side*. Anthropic appends loaded definitions inline so that the prefix cache is preserved. A server cannot do that. After `list_changed`, the client changes the tool definitions, and with Claude, “Modifying tool definitions” invalidates the entire cache. Every search would then discard the cache.
- **Inconsistent client behavior:** Claude Code updates the list interactively, but immediately defers the new tools again because of its own tool search (resulting in a two-stage search again). In `-p` mode and in the SDK, only the tool list is updated. Whether and when other clients reload after `list_changed` has to be checked per client. According to the spec, the server “SHOULD” send the notification. How clients react is not specified by the spec (https://modelcontextprotocol.io/specification/2025-11-25/server/tools#list-changed-notification).
- **Implementation:** The loaded set must be kept **per session**. Today `Hub` manages the peers globally and notifies all of them. For `/mcp` in HTTP mode with multiple clients, this would require session state in the `Gateway` instead of in the `Hub`. Like Pi, the result should say “available from your next call”, and `call_tool` must remain as a fallback.

Recommendation: at most as a separate, opt-in endpoint or mode (e.g. `/mcp/progressive`) for clients without their own tool search and without prompt caching concerns. Implement the P1 items first, which benefit all modes.

### P3: Quality assurance

- Ranking unit tests with a small corpus: name match beats description match, coverage beats single match (Pi's example `screenshot window`), exact name first, camelCase/snake_case split, plural.
- A small golden list “query → expected tool” from the real upstreams of one's own config (Executor has `e2e/benchmarks/discovery.ts` for this), as an ignored test or a subcommand `emcipi search "<query>"` analogous to `emcipi tools`.

### What is deliberately not recommended

- **Embeddings or hybrid** (Docker `find-tools`, ToolHive, VS Code): They require an external model or an API key and contradict the goal of a “small gateway”. ToolHive and Docker only offer them optionally. The BM25 branch is the robust core.
- **Regex search** (Anthropic regex variant, mcpc `-E`): For models this is less natural than keywords, which is why Anthropic additionally offers BM25. A `+term` required filter like in Claude Code's `ToolSearch` would be the cheaper alternative if the need arises.
- **Code Mode:** requires a sandbox and is out of scope for emcipi.
