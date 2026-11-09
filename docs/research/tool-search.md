# Tool-Suche in MCP-Gateways und -Clients

Stand: 2026-11-09. Ziel: Wie lösen andere Gateways und Clients „Dynamic Tool Discovery“, statt alle Tools in den Kontext zu laden? Daraus abgeleitet: eine konkrete Empfehlung für `search_tools` / `describe_tools` / `call_tool` in emcipi ohne schwere Abhängigkeiten.

Quellen sind, wo möglich, Quellcode mit festem Commit (Permalinks) oder offizielle Doku. Aussagen ohne Primärquelle sind als **unverifiziert** markiert.

Verwendete Commits:

| Repo | Commit |
|---|---|
| earendil-works/pi | `7d073328e44337d3292a7db9b0f0308f679c5e97` |
| openai/codex | `8703d32bd0ea5cf26768859a3dd476df906ba053` |
| UsefulSoftwareCo/executor (Branch `v2`) | `2f4c398f3906e0570f0e25a16fa3602329baba29` |
| 1mcp-app/agent | `c51c5228d1bd599ee8f3e443f2d2e7e50381b5a1` |
| docker/mcp-gateway | `7e0f34277c91855ad861eeff536b4fdc77420593` |
| stacklok/toolhive | `42cd9b7e497bfc77dc67a1e73037675247d405b1` |
| cloudflare/agents | `5c33d89791d6a00c6ea64cf7790fa753e3e5cbfd` |
| microsoft/vscode-copilot-chat | `5863f5a7088958050792b5dccbe8b46c6e13eccc` |
| metatool-ai/metamcp | `250240be7da37e3f21d53eaa66afac5eee775aa5` |

## Kurzfassung

- **BM25 hat sich durchgesetzt.** Anthropic (BM25-Variante), Pi, Codex, Executor, Docker (`mcp-find`) und ToolHive (FTS5) ranken lexikalisch mit Okapi BM25, fast alle mit `k1 = 1.2`, `b = 0.75` (Docker: `k1 = 1.5`). Embeddings gibt es nur optional (Docker `find-tools`, ToolHive hybrid, VS Code).
- **Der Name zählt mehr als die Beschreibung.** Pi gewichtet einen Namenstreffer mit 3×IDF, sodass er jede Zahl von Beschreibungstreffern schlägt, und sortiert zuerst nach Abdeckung der Query-Terme. Docker gewichtet über Token-Wiederholung (Servername ×4, Toolname ×2), Cloudflare über Feldgewichte.
- **Indexiert werden Name, Beschreibung, Parameternamen und Parameterbeschreibungen**, meist zusätzlich Server- bzw. Namespace-Name und -Beschreibung. Das gilt für Anthropic (offiziell dokumentiert), Pi, Codex und Executor.
- **Tokenisierung:** camelCase und snake_case werden gesplittet, es gibt einen naiven Plural-Stemmer und (bei Pi) eine kurze englische Stoppwortliste. Pi und Executor nutzen nahezu identischen Code.
- **Ergebnisgröße:** Standard 5 (Anthropic, Claude Code), 8 (Pi, Codex, ToolHive), 10 (Docker, Executor), 20 (1MCP).
- **Zwei Ergebnismodelle:** (a) Die gefundenen Tools werden für den nächsten Modellaufruf **deklariert** (Anthropic `tool_reference`, Claude Code, Pi `tool_search`, Codex, VS Code, Docker `mcp-add`). (b) Die Suche liefert Namen und Kurzinfo, aufgerufen wird über ein **Proxy-Tool** (1MCP `tool_invoke`, Executor `call`, ToolHive `call_tool`, Docker `mcp-exec`, emcipi `call_tool`). Variante (a) setzen fast nur Clients bzw. die API selbst um, weil nur sie die Tool-Liste cache-schonend erweitern können. Server-seitig (Gateway) dominiert (b).

## Vergleichstabelle

| System | Ranking | Durchsuchte Felder | Default / Max | Schema im Suchergebnis | Danach |
|---|---|---|---|---|---|
| Anthropic API, `tool_search_tool_regex_20251119` | Python-Regex (`re.search`, case-insensitive), ≤200 Zeichen | Name, Beschreibung, Argumentnamen, Argumentbeschreibungen | 5 / 10 000 | `tool_reference`, die API expandiert zur vollen Definition | Deferred Loading, inline im Verlauf, Cache bleibt erhalten |
| Anthropic API, `tool_search_tool_bm25_20251119` | BM25, natürliche Sprache, ≤500 Zeichen | wie oben | 5 / 10 000 | wie oben | wie oben |
| Claude Code `ToolSearch` | nicht öffentlich (**unverifiziert**) | Name, Beschreibung (bis 4 096 Zeichen) | 5 (beobachtet) | volle JSON-Schemas | Tool danach direkt aufrufbar |
| Pi `tool_search` | gefeldetes BM25 (k1 1.2, b 0.75, Name 3×IDF, Namespace 0.5×IDF, Abdeckung zuerst) | Name, Beschreibung, Property-Namen und -Beschreibungen rekursiv, Namespace-Name/-Beschreibung/-Instructions | 8 / – | nein (Name und erste Zeile) | `setActiveTools`, ab dem nächsten Call deklariert |
| Pi codemode `searchTools()` | gleicher Ranker | gleich | 8 / – | nein, `describeTool()` liefert die TS-Deklaration | Aufruf im Skript `tools.<name>()` |
| OpenAI Codex `tool_search` | BM25 (Crate `bm25` 2.3.2, English-Tokenizer) | Name, Name mit `_`→Leerzeichen, Server, Titel, Beschreibung, Connector, Namespace-Beschreibung, Top-Level-Property-Namen | 8 / – | ja, ladbare Specs (`defer_loading: true`) | für den nächsten Modellaufruf geladen |
| Executor `search` | BM25 in Postgres (k1 1.2, b 0.75), ein Feld | Pfad/Name, Titel, Beschreibung, Top-Level-Input-Namen und -Beschreibungen, Router-/App-Titel und -Beschreibungen, Account-Labels | 10 / Seitenbudget 25 kB | einzeilige Input-Typen (≤1 000 Zeichen); volle Signatur über `describe` | Proxy: `call` (auch Batch) oder `execute` (JS) |
| 1MCP Lazy Loading `tool_list` | kein Ranking; Glob (`*`, `?`) auf den Toolnamen, Filter nach Server/Tag | Toolname | 20 / 5 000 | nein, `tool_schema` separat | Proxy: `tool_invoke` |
| Docker MCP Gateway `mcp-find` | BM25 (k1 1.5, b 0.75), Feldgewicht über Token-Wiederholung; alternativ Embeddings | **Server**: Name ×4, Titel ×3, Toolnamen ×2, Beschreibungen ×1 | 10 / – | Config-Schema des Servers | `mcp-add` registriert die Tools (→ `tools/list_changed`), alternativ `mcp-exec` |
| Docker `find-tools` (optional) | Embeddings (`text-embedding-3-small`, OpenAI) | Tool-Embeddings | 5 fest | ja (`inputSchema`) | – |
| ToolHive vMCP Optimizer `find_tool` | hybrid: SQLite FTS5 BM25 (Porter) + Embedding-Cosinus, Anteil 0.5 | Name, Beschreibung | 8 / – | ja, volles `mcp.Tool` | Proxy: `call_tool` |
| Cloudflare Code Mode `codemode.search()` | gewichtete Feldtreffer mit Exakt-/Präfix-Boni und Mindestabdeckung | Pfad, Connector, Methode, Beschreibung | 50 | nein, `describe()` liefert TS-Doku | Aufruf im Code |
| VS Code Copilot „virtual tools“ | LLM-Gruppierung plus 10 Embedding-Treffer | Gruppen-Zusammenfassungen | ab 64 Tools | – | `activate_*` klappt Gruppe in die Tool-Liste auf |
| MetaMCP | keine Suche (Roadmap) | – | – | – | – |
| emcipi (Ist) | Substring: +3 Name, +1 Beschreibung/Titel | qualifizierter Name, Beschreibung, Titel | 10 / 50 | optional (`include_schema`) | Proxy: `call_tool` |

---

## 1. Anthropic Tool Search Tool (Claude API)

Quelle: https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-search-tool

**Varianten**

- `tool_search_tool_regex_20251119` (Name `tool_search_tool_regex`): „Claude writes Python `re.search()` patterns, not natural language queries. Matching is case-insensitive.“ Maximal 200 Zeichen pro Pattern.
- `tool_search_tool_bm25_20251119` (Name `tool_search_tool_bm25`): „Claude searches with natural language queries. Maximum query length: 500 characters.“
- Durchsuchte Felder (wörtlich): „Both tool search variants (`regex` and `bm25`) search tool names, descriptions, argument names, and argument descriptions.“

**Ablauf und `defer_loading`**

- Alle Tools werden mit vollständiger Definition im Request mitgeschickt. Tools mit `defer_loading: true` landen nicht im Kontext. Mindestens ein Tool muss nicht-deferred sein, sonst kommt HTTP 400 mit „At least one tool must have defer_loading=false.“
- Die Suche liefert `tool_reference`-Blöcke, die die API automatisch zu vollen Definitionen expandiert: „returns the matching tools as `tool_reference` blocks (up to 5 by default, although Claude can set a `limit` in its search input)“.
- `limit`: „an integer from 1 to 10,000 that caps how many matching tools the search returns (default: 5)“. Bis zu 10 000 Deferred Tools pro Request.
- Ergebnisformat: `tool_search_tool_result` → `tool_search_tool_search_result.tool_references[] = {type: "tool_reference", tool_name}`. Ohne Treffer kommt ein leeres Array und kein Fehler.
- Eine eigene Suche ist möglich: Ein Client-Tool gibt `tool_result` mit `[{ "type": "tool_reference", "tool_name": "…" }]` zurück, etwa für Embedding-Suche.
- Prompt-Caching: „Deferred tools are not included in the system-prompt prefix. When the model discovers a deferred tool through tool search, the definition is appended inline as a `tool_reference` block in the conversation history. The prefix is untouched, so prompt caching is preserved.“ Dagegen gilt: „Modifying tool definitions“ invalidiert den „Entire cache (tools, system, messages)“. Quelle: https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-use-with-prompt-caching

**Empfehlungen aus der Doku** (relevant für emcipi):

- „Keep your 3–5 most frequently used tools non-deferred.“
- „Use consistent namespacing in tool names: prefix by service or resource (for example, `github_`, `slack_`) so one search matches the whole group.“
- „Add a system prompt section describing available tool categories: ‚You can search for tools to interact with Slack, GitHub, and Jira.‘“
- Einsatz lohnt sich ab „10 or more tools“ oder wenn Tool-Definitionen „more than 10k tokens“ belegen.

**Kennzahlen** (Anthropic Engineering, „Advanced tool use“, 24.11.2025, https://www.anthropic.com/engineering/advanced-tool-use): „58 tools consuming approximately 55K tokens“. Mit Tool Search „Opus 4 improved from 49% to 74%, and Opus 4.5 improved from 79.5% to 88.1%“ in MCP-Evals mit großen Tool-Bibliotheken.

## 2. Claude Code MCP Tool Search

Quelle: https://code.claude.com/docs/en/mcp (Abschnitte „Scale with MCP tool search“, „Configure tool search“)

- Standardmäßig an: „Tool search is enabled by default: MCP tools are deferred and discovered on demand.“ Und: „Only tool names and server instructions load at session start“.
- `ENABLE_TOOL_SEARCH`:
  - (unset) / `true`: alle MCP-Tools deferred
  - `auto`: „Tools that would otherwise be deferred load upfront while their definitions total under 10% of the context window. At 10% or more, tool search turns on“
  - `auto:N`: eigener Prozentsatz, z. B. `auto:5`
  - `false`: alles upfront
- Ausnahmen: `alwaysLoad: true` pro Server in der Config, `_meta["anthropic/alwaysLoad"]` pro Tool (vom Server-Autor gesetzt).
- Für Server-Autoren: „the server instructions field becomes more useful with tool search enabled. Server instructions help Claude understand when to search for your tools“. Die Instructions sollen erklären: „What category of tasks your tools handle / When Claude should search for your tools / Key capabilities your server provides“.
- Beschreibungslängen: 4 096 Zeichen für Instructions und upfront geladene Tools, 16 384 für über die Suche geladene Tools. „When tool search matches Claude's query against a tool's description, it reads only the part within the shorter limit, so put the words Claude would search for near the start too.“
- `list_changed`: „In an interactive terminal session, Claude Code fetches the updated list from that server“. Im `-p`-Modus und im Agent SDK „refreshes only the tool list on these notifications“.

**Das Meta-Tool selbst** ist nicht offiziell dokumentiert. Die folgende Definition stammt aus einer laufenden Claude-Code-/Agent-SDK-Session (Stand 2026-11-09) und ist nur **beobachtet**, nicht offiziell bestätigt:

> „Fetches full schema definitions for deferred tools so they can be called. Deferred tools appear by name in <system-reminder> messages. Until fetched, only the name is known — there is no parameter schema, so the tool cannot be invoked. This tool takes a query, matches it against the deferred tool list, and returns the matched tools' complete JSONSchema definitions inside a <functions> block. […] Query forms: `"select:Read,Edit,Grep"` — fetch these exact tools by name; `"notebook jupyter"` — keyword search, up to max_results best matches; `"+slack send"` — require "slack" in the name, rank by remaining terms“

Parameter: `query` und `max_results` (Default 5). Den Ranking-Algorithmus legt Claude Code nicht offen (**unverifiziert**). Interessant für emcipi sind die drei Query-Formen: exakte Auswahl, Keyword-Suche und Pflicht-Term im Namen.

**Konsequenz für emcipi:** Claude Code defert MCP-Tools ohnehin. Hängt Claude Code an `/mcp/dynamic`, sucht das Modell zweistufig (Claude-Code-`ToolSearch` lädt `search_tools`, dann sucht `search_tools`). Hängt es an `/mcp/all`, übernimmt Claude Code die Suche selbst, kostet dann aber pro Tool eine Namenszeile im Kontext. Bei sehr vielen Tools spricht weiter einiges für `dynamic`. Das ist eine Abwägung, kein Fehler.

## 3. Pi (earendil-works/pi, packages/coding-agent)

Docs: [mcp.md](https://github.com/earendil-works/pi/blob/7d073328e44337d3292a7db9b0f0308f679c5e97/packages/coding-agent/docs/mcp.md), [codemode.md](https://github.com/earendil-works/pi/blob/7d073328e44337d3292a7db9b0f0308f679c5e97/packages/coding-agent/docs/codemode.md), [cli.md](https://github.com/earendil-works/pi/blob/7d073328e44337d3292a7db9b0f0308f679c5e97/packages/coding-agent/docs/cli.md)

### Exposure-Modi (mcp.md, „Control tool exposure“)

| Exposure | Verhalten laut Doku |
|---|---|
| `codemode` (Default) | „Callable from `codemode` scripts, but neither declared to the model nor listed in the codemode description. Scripts find tools with `searchTools()`, `describeTool()`, or `ALL_TOOLS`.“ |
| `deferred` | „Not declared until `tool_search` loads a match for the next model call.“ |
| `direct` | „Declared to the model like a built-in tool and also callable from codemode.“ |
| `hidden` | „Registered but unreachable.“ |

`toolExposure` überschreibt pro Tool, auch mit `*`-Mustern. Server mit `codemode`/`deferred` landen in einer `mcp_servers`-Sektion des System-Prompts mit einer Zeile aus `description` oder der ersten Zeile der Server-Instructions. Die Sektion ist auf 250 Zeichen pro Server und 4 096 Zeichen insgesamt begrenzt ([mcp/index.ts L159–L164](https://github.com/earendil-works/pi/blob/7d073328e44337d3292a7db9b0f0308f679c5e97/packages/coding-agent/src/extensions/mcp/index.ts#L159-L164)). Ändert sie sich, hängt Pi sie an den Verlauf an, „instead of changing tool declarations, so earlier messages stay cached“ (mcp.md). Der Einleitungstext ([L167–L172](https://github.com/earendil-works/pi/blob/7d073328e44337d3292a7db9b0f0308f679c5e97/packages/coding-agent/src/extensions/mcp/index.ts#L167-L172)):

> „MCP servers whose tools are not declared to you. Call the tools of `codemode` servers from codemode scripts. Load the tools of `tool_search` servers with `tool_search`.“

### Ranking (Quellcode)

Datei: [`src/extensions/tool-search/tool.ts`](https://github.com/earendil-works/pi/blob/7d073328e44337d3292a7db9b0f0308f679c5e97/packages/coding-agent/src/extensions/tool-search/tool.ts). `tool_search` und codemode-`searchTools()` nutzen denselben Ranker ([execute.ts L669–L697](https://github.com/earendil-works/pi/blob/7d073328e44337d3292a7db9b0f0308f679c5e97/packages/coding-agent/src/extensions/codemode/execute.ts#L669-L697)).

- **Tokenizer** (L78–L93): NFC, camelCase-Split per `([a-z0-9])([A-Z])` → `$1 $2` und `([A-Z]+)([A-Z][a-z])` → `$1 $2`, lowercase, alles außer Buchstaben/Ziffern wird Leerzeichen, für Nicht-ASCII `Intl.Segmenter`. Danach Stoppwörter raus und Stemming.
- **Stoppwörter** (L45–L67): `a an and are as at be by for from in is it of on or that the this to with`.
- **Stemmer** (L70–L75): „Naive singular form“, `ies`→`y` (Länge >4), `ches|shes|sses|xes|zes` → `-es` (Länge >4), sonst `s` ab (Länge >3, nicht `ss`).
- **Dokument** (L104–L141): `text` = Beschreibung plus rekursiv alle Schema-`description`s und Property-Namen (folgt lokalen `$ref`s, `items`, `anyOf/oneOf/allOf`). Der Name ist ein eigenes Feld. `namespaceText` = Servername + Beschreibung + Instructions.
- **Score** (L191–L261):
  - IDF: `ln(1 + (N − df + 0.5) / (df + 0.5))`, df zählt Treffer in Name, Text oder Namespace
  - pro Query-Term: `idf × (3·[Term im Namen] + 0.5·[Term im Namespace] + tf·(k1+1) / (tf + k1·(1 − b + b·len/avglen)))`, `k1 = 1.2`, `b = 0.75`. Nur die Beschreibung wird längennormalisiert.
  - Kommentar zu `NAME_WEIGHT = 3`: „The text part of a term is at most `k1 + 1` (2.2) times its IDF and about 1 for a single mention, so a name match beats any number of mentions in the description.“
  - `NAMESPACE_WEIGHT = 0.5`: „They are shared by every tool of the server, so they help find the server but not the tool in it.“
  - Bonus: Kommen alle Terme des „lokalen“ Namens (letztes `__`-Segment) in der Query vor, gibt es zusätzlich die Summe aller IDFs.
  - Sortierung: zuerst **exakter Name** (ganze Query = Name oder `__`-Suffix), dann **Abdeckung** (Anzahl getroffener Query-Terme), dann Score. Aus dem Kommentar: „for `screenshot window`, a tool whose description has both words beats `window`, whose name has one“.
- **Default-Limit** 8 (L22), kein Maximum.

### `tool_search` (Meta-Tool)

Beschreibung (L320), wörtlich:

> „# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nSome of the tools, such as tools of MCP servers, may not have been provided to you upfront, and you should use this tool (`tool_search`) to search for the required tools. For MCP tool discovery, always use `tool_search`.“

Parameter (L264–L269): `query` („Search query for deferred tools.“), `limit` („Maximum number of tools to return. Defaults to 8.“). Die Beschreibung ist absichtlich statisch: „It does not list the searchable tools or their namespaces, so it stays the same while tools are registered“ (L316–L319).

Ergebnis (L340–L345): `Loaded N tools. They are available from your next call:` und dann `- name: erste Zeile der Beschreibung`. Treffer werden per `setActiveTools([...active, ...matches])` aktiviert (L300–L314), stehen im Transkript und überleben `/tree`, Resume und Fork.

### codemode

Globals laut codemode.md: `searchTools(query, { limit?, namespace? })` „Rank callable tools by relevance (BM25, default limit 8). Resolves to `{ name, description }[]`.“, `describeTool(name)` „Resolves to a tool's description and TypeScript declaration“, `describeNamespace(name)`, `ALL_TOOLS`. Die codemode-Beschreibung listet deklarierte Tools mit TS-Deklarationen bis zu einem Budget von 3 000 geschätzten Tokens. Deferred Tools tauchen nicht auf, damit die Beschreibung stabil bleibt. Hinweiszeile in der Tool-Beschreibung ([codemode/tool.ts L133](https://github.com/earendil-works/pi/blob/7d073328e44337d3292a7db9b0f0308f679c5e97/packages/coding-agent/src/extensions/codemode/tool.ts#L128-L139)):

> „- `ALL_TOOLS`, `await searchTools(query, { limit?, namespace? })`, `await describeTool(name)`, `await describeNamespace(name)`: find unlisted tools, such as MCP tools.“

## 4. OpenAI Codex (`tool_search`, Rust)

Codex ist ein Rust-Client mit `rmcp` und deshalb besonders nah an emcipi.

- Ranking: Crate [`bm25`](https://crates.io/crates/bm25) Version 2.3.2 ([Cargo.toml L367](https://github.com/openai/codex/blob/8703d32bd0ea5cf26768859a3dd476df906ba053/codex-rs/Cargo.toml#L367)), `EmbedderBuilder::with_fit_to_corpus(Language::English, &corpus)` und `Scorer::upsert`/`matches` ([tool_search.rs L166–L175, L334–L347](https://github.com/openai/codex/blob/8703d32bd0ea5cf26768859a3dd476df906ba053/codex-rs/core/src/tools/handlers/tool_search.rs#L166-L175)). Es gibt ein einzelnes Textfeld, keine Feldgewichte. Das Crate bringt mit dem Default-Feature `default_tokenizer` die Abhängigkeiten `rust-stemmers`, `stop-words`, `deunicode`, `unicode-segmentation` und `cached` mit (crates.io-Metadaten 2.3.2).
- Suchtext für MCP-Tools ([mcp.rs L603–L650](https://github.com/openai/codex/blob/8703d32bd0ea5cf26768859a3dd476df906ba053/codex-rs/core/src/tools/handlers/mcp.rs#L603-L650)): flacher Toolname, callable name, Original-Toolname, Servername, Titel, Beschreibung, Connector-Name, Namespace-Beschreibung, Plugin-Namen, sortierte Top-Level-Property-Namen. Bei Nicht-MCP-Funktionen zusätzlich `name.replace('_', " ")` und rekursiv die Schema-Beschreibungen ([tools/src/tool_search.rs L171–L196](https://github.com/openai/codex/blob/8703d32bd0ea5cf26768859a3dd476df906ba053/codex-rs/tools/src/tool_search.rs#L171-L196)).
- `TOOL_SEARCH_DEFAULT_LIMIT = 8` ([tool_discovery.rs L7](https://github.com/openai/codex/blob/8703d32bd0ea5cf26768859a3dd476df906ba053/codex-rs/tools/src/tool_discovery.rs#L7)).
- Deferral: Ist die Suche aktiv, werden alle MCP-Tools `ToolExposure::Deferred` ([mcp_tool_exposure.rs L158–L162](https://github.com/openai/codex/blob/8703d32bd0ea5cf26768859a3dd476df906ba053/codex-rs/core/src/mcp_tool_exposure.rs#L158-L162)). Treffer kommen als ladbare Namespace-Specs mit `defer_loading = Some(true)` zurück und werden für den nächsten Modellaufruf geladen ([tool_search.rs L98–L132](https://github.com/openai/codex/blob/8703d32bd0ea5cf26768859a3dd476df906ba053/codex-rs/tools/src/tool_search.rs#L98-L132)).
- Beschreibung ([tool_search_spec.rs L93–L96](https://github.com/openai/codex/blob/8703d32bd0ea5cf26768859a3dd476df906ba053/codex-rs/core/src/tools/handlers/tool_search_spec.rs#L93-L96)):

> „# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nYou have access to tools from the following sources:\n- <source>: <description>\n…\nSome of the tools may not have been provided to you upfront, and you should use this tool (`tool_search`) to search for the required tools. For MCP tool discovery, always use `tool_search` instead of `list_mcp_resources` or `list_mcp_resource_templates`.“

  Parameter: `query` („Search query for deferred tools.“), `limit` („Maximum number of tools to return. Defaults to 8.“). Pis Text ist erkennbar davon abgeleitet. Pi verweist im Code selbst auf Codex: „as Codex allows for deferred namespaces“ (mcp/index.ts L158).

## 5. Executor (UsefulSoftwareCo/executor)

Executor ist ein Gateway mit einem einzigen MCP-Endpunkt für viele „Apps“ (OpenAPI, MCP, GraphQL). Docs: [apps/docs/content/mcp.md](https://github.com/UsefulSoftwareCo/executor/blob/2f4c398f3906e0570f0e25a16fa3602329baba29/apps/docs/content/mcp.md). „The endpoint does not expose one MCP tool per app tool. However many apps you add, the tool list stays the same size.“ Die Tools sind `skills`, `search`, `describe`, `call`, `execute` und `resume`. Modi gibt es für Elicitation (model/browser/native), nicht für die Tool-Exposure.

**Ranking** ([packages/sdk/src/implementation/tool-search.ts](https://github.com/UsefulSoftwareCo/executor/blob/2f4c398f3906e0570f0e25a16fa3602329baba29/packages/sdk/src/implementation/tool-search.ts), [tool-search-store.ts](https://github.com/UsefulSoftwareCo/executor/blob/2f4c398f3906e0570f0e25a16fa3602329baba29/packages/sdk/src/implementation/tool-search-store.ts)):

- `words()` (L24–L38): camelCase-Split (gleiche Regexes wie Pi), lowercase, Split an Nicht-Buchstaben/Ziffern, Stemmer `ies`→`y`, `s` ab. Keine Stoppwörter.
- Felder (L66–L82): Root-Router-Titel und -Beschreibung, `tool.name`, `tool.title`, `tool.description`, Top-Level-Input-Property-Namen und -Beschreibungen (L44–L60), Router-Titel und -Beschreibung, dazu die Labels des Targets bzw. Accounts. Alles landet in einem Bag of Words, ohne Feldgewichte.
- Okapi BM25 in SQL, `const BM25 = { k1: 1.2, b: 0.75 }` (Store L98, Formel L359–L363). Ohne Query-Terme gilt die Sortierung nach Target und Name.

**Limits** ([packages/mcp/src/contracts/execute.ts L359–L364, L496](https://github.com/UsefulSoftwareCo/executor/blob/2f4c398f3906e0570f0e25a16fa3602329baba29/packages/mcp/src/contracts/execute.ts#L359-L364)): `defaultItems: 10`, `descriptionChars: 200` (erste Zeile), `inputChars: 1_000` (einzeiliger Input-Typ, sonst `inputTruncated`), `describePaths: 20`, Seitenbudget `searchPageBytes = 25_000`. Ein Treffer enthält `path`, `description` (erste Zeile), `input` (Typ in einer Zeile). App und Accounts werden einmal pro Ergebnis als `namespaces` gelistet, nicht in jedem Item. Paging über `next`.

**Unbekannte Pfade:** `describe` und `call` liefern „the closest paths“ per Levenshtein ([packages/mcp/src/implementation/suggestions.ts](https://github.com/UsefulSoftwareCo/executor/blob/2f4c398f3906e0570f0e25a16fa3602329baba29/packages/mcp/src/implementation/suggestions.ts)).

**Meta-Tool-Beschreibungen** (execute.ts L367–L385, L524–L548), wörtlich:

- `search`: „Find Executor app tools by words in their paths, descriptions and labels. Returns exact paths, one-line descriptions and input types, with each app and its linked accounts listed once. Pass a path to call or describe, or use it as tools.<path> inside execute. Omit query to list a namespace. To read the following page, call search again with the returned next object as the whole input; next is null on the last page.“
  - `query`: „Words matched against tool paths, descriptions, input names, and app, account and router labels. Omit it to list a namespace.“
  - `namespace`: „App slug, such as axiom, or a namespace inside it, such as acme.issues.“
  - `limit`: „Most items to return, 10 by default. A page ends sooner when it reaches its size budget.“
- `describe`: „Read the whole description and TypeScript signature, with input and output types, of tools at exact paths from search. Unknown paths are listed in missing with the closest paths.“
- `call`: „Call Executor app tools directly by exact path from search. Pass one {path, input} as calls, or an array of independent calls to run them concurrently; results come back in the same order, each with ok and its value or error. Use execute instead when one call's output feeds another, or to filter a large result before it reaches you. […]“
- `execute`: „Run a JavaScript program over Executor apps, for work where one result feeds another call or a large result should be filtered before it reaches you; for one or a few independent calls, use call instead. Find tools with the search tool or tools.search inside the program, and full signatures with describe or tools.search.describe({ paths }). […]“

## 6. 1MCP (1mcp-app/agent), „OneMCP“

Unter dem Namen „OneMCP“ gibt es mehrere Projekte. Das Aggregator-Gateway mit mehreren Exposure-Modi ist **1MCP** (`1mcp-app/agent`). `Gentoro-OneMCP/onemcp` ist dagegen eine LLM-gestützte Natural-Language-API-Laufzeit („exposes your system through a single, natural-language interface“, [README](https://github.com/Gentoro-OneMCP/onemcp)) und für die Tool-Suche nicht relevant.

1MCP hat drei Modi:

1. **Direct** (Default): alle Tools durchgereicht.
2. **Lazy Loading** (`1mcp serve --enable-lazy-loading`): „Opt into meta-tool exposure for progressive tool discovery“ ([docs serve.md](https://github.com/1mcp-app/agent/blob/c51c5228d1bd599ee8f3e443f2d2e7e50381b5a1/docs/site/en/commands/serve.md)). README L162: „It keeps the backend discovery and invocation surface at `tool_list`, `tool_schema`, and `tool_invoke`“.
3. **CLI Mode**: Der Agent nutzt Shell-Befehle `1mcp instructions`, `1mcp inspect <server>`, `1mcp inspect <server>/<tool>` und `1mcp run <server>/<tool> --args '<json>'` („This is progressive disclosure for tools.“, [cli-mode.md](https://github.com/1mcp-app/agent/blob/c51c5228d1bd599ee8f3e443f2d2e7e50381b5a1/docs/site/en/guide/integrations/cli-mode.md)).

**Suche:** Es gibt kein Ranking. `tool_list` filtert nach `server`, `tag` und `pattern`. Das Pattern ist ein Glob nur auf dem Toolnamen (`*`→`.*`, `?`→`.`, verankert `^…$`). Default-Seitengröße 20, Maximum 5 000 ([toolRegistry.ts L209–L274](https://github.com/1mcp-app/agent/blob/c51c5228d1bd599ee8f3e443f2d2e7e50381b5a1/src/core/capabilities/toolRegistry.ts#L209-L274)). Die CLI-Suche (`inspect --search`) macht Substring- oder Glob-Matching auf `server/tool`, mit `--include-descriptions` auch auf der Beschreibung ([inspectSearch.ts L86–L91, L119–L121](https://github.com/1mcp-app/agent/blob/c51c5228d1bd599ee8f3e443f2d2e7e50381b5a1/src/core/capabilities/inspectSearch.ts#L86-L91)).

Beschreibungen ([lazyTools.ts](https://github.com/1mcp-app/agent/blob/c51c5228d1bd599ee8f3e443f2d2e7e50381b5a1/src/core/capabilities/internal/lazyTools.ts)), wörtlich:

- `tool_list`: „List all available MCP tools with names and descriptions. Use for tool discovery.“ (Input `server?`, `pattern?`, `tag?`, `limit?`, `cursor?`)
- `tool_schema`: „Get the full schema for a specific tool including input validation rules“ (Input `server`, `toolName`)
- `tool_invoke`: „Execute any tool on any MCP server with proper argument validation“ (Input `server`, `toolName`, `args`)

## 7. Docker MCP Gateway (dynamic tools)

Spec: [docs/feature-specs/dynamic_servers.md](https://github.com/docker/mcp-gateway/blob/7e0f34277c91855ad861eeff536b4fdc77420593/docs/feature-specs/dynamic_servers.md). Aktiviert per `docker mcp feature enable dynamic-tools`. Docker sucht **Server im Katalog**, nicht einzelne Tools.

- `mcp-find` ([dynamic_mcps.go L19–L46](https://github.com/docker/mcp-gateway/blob/7e0f34277c91855ad861eeff536b4fdc77420593/pkg/gateway/dynamic_mcps.go#L19-L46)):
  > „Find MCP servers in the current catalog by name, title, or description.
  > If the user is looking for new capabilities, use this tool to search the MCP catalog for servers that should potentially be enabled.
  > This will not enable the server but will return information about servers that could be enabled.
  > If we find an mcp server, it can be added with the mcp-add tool, and configured with mcp-config-set.“

  `query` („Search query to find servers by name, title, or description (case-insensitive)“), `limit` (Default 10).
- Ranking ohne Embeddings: BM25 ([findmcps.go L76–L196](https://github.com/docker/mcp-gateway/blob/7e0f34277c91855ad861eeff536b4fdc77420593/pkg/gateway/findmcps.go#L76-L196)). Ein Dokument pro Server, „BM25F approximation via token repetition“: Servername ×4, Titel ×3, Toolnamen ×2, Beschreibung und Tool-Beschreibungen ×1. Laut Kommentar auch Tags/Kategorie ×3, im Code fehlen diese. `k1 = 1.5`, `b = 0.75`. Der Tokenizer splittet nur an Nicht-`[a-z0-9]`, ohne camelCase-Split und ohne Stemming. Der Index wird bei jeder Query neu gebaut (L200–L207). Mit Embedding-Client (Feature `use-embeddings` plus `OPENAI_API_KEY`) wird stattdessen per Vektor gesucht ([reload.go L98–L103](https://github.com/docker/mcp-gateway/blob/7e0f34277c91855ad861eeff536b4fdc77420593/pkg/gateway/reload.go#L93-L110)).
- `mcp-add` (`name`, `activate` Default `true`): „Add a new MCP server to the session.“ Bei `activate` ruft der Handler `updateServerCapabilities` → `mcpServer.AddTool`/`RemoveTools` auf ([mcpadd.go L241–L261](https://github.com/docker/mcp-gateway/blob/7e0f34277c91855ad861eeff536b4fdc77420593/pkg/gateway/mcpadd.go#L241-L261)). Im Go-SDK lösen beide `notifications/tools/list_changed` aus ([go-sdk server.go L385, L639](https://github.com/modelcontextprotocol/go-sdk/blob/df0f29525bc6c590b6b68d9aae581ec0f21ebb3e/mcp/server.go#L385)). Docker ist damit das einzige untersuchte **Gateway**, das gefundene Tools per `list_changed` nachlädt.
- `mcp-exec`: „Execute a tool that exists in the current session. This allows calling tools that may not be visible in listTools results.“ (`name`, `arguments`)
- `find-tools` (nur mit Embeddings): „Analyze a task description and recommend relevant MCP tools that could help accomplish it. Uses AI to intelligently match your needs to available tools.“ Nutzt `text-embedding-3-small` mit fester Trefferzahl 5 und liefert `{tools: [{name, description, inputSchema}]}` im `tools/list`-Format ([findtools.go L39, L93, L131–L140, L150](https://github.com/docker/mcp-gateway/blob/7e0f34277c91855ad861eeff536b4fdc77420593/pkg/gateway/findtools.go#L146-L160)).
- `code-mode`: erzeugt ein neues Tool `code-mode-<name>`, das JS über die Tools mehrerer Server ausführt ([dynamic_mcps.go L154–L182](https://github.com/docker/mcp-gateway/blob/7e0f34277c91855ad861eeff536b4fdc77420593/pkg/gateway/dynamic_mcps.go#L154-L182), codemode.go L133 `AddTool`).

## 8. Weitere Gateways, Proxies und Clients

### ToolHive vMCP Optimizer (stacklok/toolhive)

Das frühere Projekt `StacklokLabs/mcp-optimizer` ist archiviert. Die Funktion steckt jetzt in `pkg/vmcp/optimizer`. „When the optimizer is enabled, vMCP exposes only two tools to clients: find_tool […] call_tool“ ([optimizer.go L1–L12](https://github.com/stacklok/toolhive/blob/42cd9b7e497bfc77dc67a1e73037675247d405b1/pkg/vmcp/optimizer/optimizer.go#L1-L12)).

- Ranking: hybrid. SQLite FTS5 mit `tokenize='porter'` auf `name` und `description` ([schema.sql](https://github.com/stacklok/toolhive/blob/42cd9b7e497bfc77dc67a1e73037675247d405b1/pkg/vmcp/optimizer/internal/toolstore/schema.sql)) plus optional Embedding-Cosinus. Defaults: `DefaultMaxToolsToReturn = 8`, `DefaultHybridSemanticToolsRatio = 0.5`, `DefaultSemanticDistanceThreshold = 1.0` ([sqlite_store.go L34–L46](https://github.com/stacklok/toolhive/blob/42cd9b7e497bfc77dc67a1e73037675247d405b1/pkg/vmcp/optimizer/internal/toolstore/sqlite_store.go#L34-L46)). FTS-Terme werden OR-verknüpft. Als Keyword-Quelle dient `tool_keywords`, sonst die Wörter aus `tool_description` (L553–L555).
- Ergebnis: volle `mcp.Tool` inklusive Input-Schema, dazu `token_metrics` (optimizer.go L395–L403).
- Beschreibungen ([decorator.go L62–L87](https://github.com/stacklok/toolhive/blob/42cd9b7e497bfc77dc67a1e73037675247d405b1/pkg/vmcp/session/optimizerdec/decorator.go#L62-L87)):
  - `find_tool`: „Find and return tools that can help accomplish the user's request. This searches available MCP server tools using semantic and keyword-based matching. Use this function when you need to: (1) discover what tools are available for a specific task, (2) find the right tool(s) before attempting to solve a problem, (3) check if required functionality exists in the current environment. Returns matching tools ranked by relevance including their names, descriptions, required parameters and schemas, plus token efficiency metrics showing baseline_tokens, returned_tokens, and savings_percent. Always call this before call_tool to discover the correct tool name and parameter schema.“
    - `tool_description`: „Description of the task or capability needed (e.g. 'web search', 'analyze CSV file', 'send an email'). This is used for semantic similarity matching against available tools.“
    - `tool_keywords`: „Optional keywords driving the BM25 keyword-search arm (e.g. ['list', 'issues', 'github'] or ['SQL', 'query', 'postgres']). […]“
  - `call_tool`: „Execute a specific tool with the provided parameters. Use this function to run a tool after identifying it with find_tool. Important: always use find_tool first to get the correct tool_name and parameter schema before calling this function.“
  - Robustheit: `call_tool` akzeptiert auch ein `tool_name`, das das Modell fälschlich in `parameters` verschachtelt hat (optimizer.go, `resolveCallToolTarget`).

### VS Code Copilot Chat („virtual tools“)

[virtualToolsConstants.ts](https://github.com/microsoft/vscode-copilot-chat/blob/5863f5a7088958050792b5dccbe8b46c6e13eccc/src/extension/tools/common/virtualTools/virtualToolsConstants.ts): Ab `START_GROUPING_AFTER_TOOL_COUNT = HARD_TOOL_LIMIT / 2` („64, currently“) werden Tools gruppiert. Die Gruppen werden zu virtuellen Tools mit dem Präfix `activate_` und einer LLM-generierten Zusammenfassung. Zusätzlich gibt es `NUM_EMBED_MATCHED_TOOLS = 10` per Embedding passend zur Nutzeranfrage. Beschreibungsrahmen der Gruppen-Tools ([virtualToolGrouper.ts L29–L30](https://github.com/microsoft/vscode-copilot-chat/blob/5863f5a7088958050792b5dccbe8b46c6e13eccc/src/extension/tools/common/virtualTools/virtualToolGrouper.ts#L29-L30)):

> „Call this tool when you need access to a new category of tools. The category of tools is described as follows:\n\n<summary>\n\nBe sure to call this tool if you need a capability related to the above.“

### MetaMCP (metatool-ai/metamcp)

MetaMCP hat keine Tool-Suche. Im README stehen „Use as Elasticsearch for MCP tool selection (coming soon)“ (L85) und auf der Roadmap „Dynamically apply search rules on MetaMCP endpoints“ (L475) ([README](https://github.com/metatool-ai/metamcp/blob/250240be7da37e3f21d53eaa66afac5eee775aa5/README.md)). Vorhanden ist eine Middleware „filter-inactive-tools“, also statisches Filtern.

### apify/mcpc

`mcpc grep <pattern>` „searches tools and server instructions“, case-insensitive als Substring oder mit `-E` als Regex, `-m` begrenzt die Treffer. Ausdrücklich als „dynamic tool discovery“ gedacht ([README](https://github.com/apify/mcpc/blob/6c30b7ca17d82151e1605d8cb22d67d15602132d/README.md)).

### Nicht gefunden

Bei `mcpjungle/MCPJungle`, `mcp-router/mcp-router` (archiviert), `sitbon/magg` und `IBM/mcp-context-forge` lieferte die GitHub-Codesuche nach `search_tools`, `find_tool`, `tool_search` und `semantic_search` keine agentenseitige Tool-Suche. Bei context-forge gibt es nur Admin-/Test-Treffer. Die Codesuche ist unvollständig, daher gilt das nur als schwaches Indiz.

## 9. Code-Execution-Ansätze (kurz)

- **Cloudflare Code Mode.** Ursprünglich: „Convert the MCP tools into a TypeScript API, and then ask an LLM to write code that calls that API.“ Damals galt: „Currently, the entire API is loaded“ (https://blog.cloudflare.com/code-mode/, 26.09.2025). Später kam der Cloudflare-MCP-Server mit nur `search()` („Search the Cloudflare OpenAPI spec. All $refs are pre-resolved inline.“) und `execute()` („Execute JavaScript code against the Cloudflare API.“) für über 2 500 Endpunkte in „around 1,000 tokens“ dazu. Das Modell filtert die Spec dabei selbst per JavaScript (https://blog.cloudflare.com/code-mode-mcp/, 20.02.2026). Im SDK hat `codemode.search()` ein eigenes Ranking: „Executor-style ranked search“ mit Feldgewichten path 12, method 10, connector 8, description 5. Feld exakt gleich Query zählt ×14, Präfix ×9, Phrase ×6. Pro Token gilt: exakt ×4, Präfix ×2, Substring ×1. Die Mindestabdeckung liegt bei 100 % für ≤2 Terme und sonst 60 %, volle Abdeckung bringt +25. Limit 50 ([connectors/search.ts](https://github.com/cloudflare/agents/blob/5c33d89791d6a00c6ea64cf7790fa753e3e5cbfd/packages/codemode/src/connectors/search.ts#L11-L161)).
- **Anthropic „Code execution with MCP“** (04.11.2025, https://www.anthropic.com/engineering/code-execution-with-mcp): Tools liegen als Dateibaum unter `./servers/`. Ein optionales `search_tools` hat einen „detail level parameter“ mit den Stufen „name only, name and description, or the full definition with schemas“. Der Beispielfall schrumpft von „150,000 tokens to 2,000 tokens“.
- **Pi codemode** und **Executor `execute`** (siehe oben) kombinieren beides: eine Suche als Funktion im Sandbox-Skript und Tools als Funktionen.

Für emcipi (kleines Rust-Gateway ohne JS-Sandbox) ist Code Mode außer Scope. Übertragbar ist die **Detailstufe** im Suchergebnis.

---

## Empfehlung für emcipi

Priorisiert nach Nutzen pro Aufwand. Alles ohne neue Abhängigkeiten umsetzbar.

### P1: Gefeldetes BM25 statt Substring-Score

Vorbild ist Pis `Bm25Ranker`, der in diesem Vergleich am sorgfältigsten begründet ist. Mit rund 150 Zeilen Rust lässt er sich direkt übertragen. Das Crate `bm25` (Codex) ist eine Alternative, kennt aber nur ein Feld und zieht mit dem Default-Tokenizer fünf Abhängigkeiten nach sich. Eine eigene Implementierung ist kleiner und erlaubt Feldgewichte.

**Felder pro Tool:**

| Feld | Inhalt | Gewicht |
|---|---|---|
| `name` | Terme des qualifizierten Namens (`server__tool`), dazu als Teilmenge `local` = Terme des Toolnamens nach dem Separator | Treffer: `3 × IDF` (binär, nicht längennormalisiert) |
| `text` | Titel, volle Beschreibung (nicht nur der erste Satz), rekursiv alle `properties`-Namen und `description`s des Input-Schemas (inklusive `items`, `anyOf/oneOf/allOf`, lokale `$ref` mit Zyklenschutz) | Okapi BM25, `k1 = 1.2`, `b = 0.75`, Längennormalisierung nur hier |
| `namespace` | Servername, `description` aus der Config, Server-Instructions des Upstreams (falls vorhanden) | Treffer: `0.5 × IDF` (binär) |

- IDF: `ln(1 + (N − df + 0.5) / (df + 0.5))`, df = Anzahl der Tools mit dem Term in irgendeinem Feld (wie Pi). N ist die Menge nach dem `server`-Filter.
- Score pro Tool: `Σ_t idf(t) × (3·inName + 0.5·inNamespace + tf·2.2 / (tf + 1.2·(0.25 + 0.75·len/avglen)))`.
- Bonus: Sind alle `local`-Terme in der Query enthalten, kommt `Σ idf` dazu (Pi).
- **Sortierung:** (1) exakter Name: die normalisierte Query (lowercase, Nicht-`[a-z0-9_]`→`_`) ist gleich dem qualifizierten Namen oder endet als dessen Suffix nach dem Separator. (2) **Anzahl abgedeckter Query-Terme**, absteigend. (3) Score. (4) Name als stabiler Tie-Breaker.
- Ergebnis: nur Tools mit Score > 0. Eine leere Query mit `server` listet weiterhin alle Tools des Servers (bestehendes Verhalten, auch bei Executor üblich).
- Index: einmal pro Tool-Liste aufbauen und bei `list_changed`/Reconnect verwerfen. Bei einigen hundert Tools ist auch ein Neuaufbau pro Query vertretbar (Docker macht das).

**Begründung:** Der heutige Score zählt einen Treffer in einer langen Beschreibung genauso wie einen in einer kurzen, ignoriert, wie selten ein Term ist (IDF), und durchsucht keine Parameter. Gerade Parameternamen (`repo`, `issue_number`, `branch`) treffen Nutzer-Formulierungen oft genauer als die Beschreibung. Anthropic durchsucht sie offiziell, Pi, Codex und Executor ebenfalls. Die Sortierung nach Abdeckung zuerst verhindert, dass ein einzelner Namenstreffer ein Tool schlägt, das alle Query-Wörter enthält.

### P1: Tokenisierung

Gleiche Funktion für Query und Dokumente:

1. camelCase/PascalCase splitten: `([a-z0-9])([A-Z])` → `$1 $2`, dann `([A-Z]+)([A-Z][a-z])` → `$1 $2` (also `getHTTPResponse` → `get http response`). In Rust geht das ohne Regex-Crate mit einer Zeichenschleife.
2. Lowercase. Jedes Zeichen, das nicht `char::is_alphanumeric` ist, wird Trenner. Damit sind snake_case, kebab-case, `.`, `/` und der Separator `__` erledigt.
3. Stoppwörter entfernen (Pis Liste: `a an and are as at be by for from in is it of on or that the this to with`).
4. Naives Singular-Stemming nur für ASCII-Wörter: `ies`→`y` (Länge >4), `ches|shes|sses|xes|zes` → `-es` (Länge >4), sonst `s` ab (Länge >3, nicht auf `ss`). Das ersetzt das heutige `strip_suffix('s')` mit `stem.len() >= 3`, das z. B. `status`→`statu` erzeugt. Pi und Executor haben das gleiche Problem bei `status`. Konsistent auf beiden Seiten angewendet ist das harmlos.
5. Die Mindestlänge 2 entfällt. Einbuchstabige Tokens sind selten, und Stoppwörter erledigen das Rauschen.

Optional (P2): ein Präfix-Fallback wie bei Cloudflare. Hat ein Query-Term (≥4 Zeichen) keinen exakten Treffer, zählt ein Dokument-Term, der mit ihm beginnt, mit halbem Gewicht (`repo` → `repository`, `config` → `configuration`). Das ist billig und fängt die häufigsten Fälle, für die man sonst Embeddings bräuchte.

### P1: Ergebnisformat

Kompakt halten, aber so, dass ein Aufruf oft ohne `describe_tools` gelingt. Executor und 1MCP machen das ähnlich (einzeiliger Input-Typ bzw. Anzahl Pflicht- und optionaler Argumente).

```
3 of 17 matching tools (github: GitHub: Issues, Pull Requests, Repositories, Code-Suche)
- github__create_issue(owner, repo, title, body?, labels?): Create a new issue in a GitHub repository.
- github__update_issue(owner, repo, issue_number, state?, title?, …): Update an existing issue.
- github__add_issue_comment(owner, repo, issue_number, body): Add a comment to an issue.

Call `describe_tools` for full input schemas, then `call_tool`.
```

- Signatur: Top-Level-Property-Namen, optionale mit `?`, abgeschnitten nach etwa 120 Zeichen mit `…`.
- Beschreibung: erste Zeile, gekappt auf etwa 200 Zeichen (Executor `descriptionChars: 200`). Das heutige `first_sentence` bleibt als Fallback sinnvoll.
- Server-Beschreibung einmal pro vorkommendem Server statt pro Tool (Executor `namespaces`).
- **Default-Limit 8** (Pi, Codex, ToolHive), Maximum 50 bleibt. Anthropic nutzt 5, Docker und Executor 10. 8 ist ein guter Mittelwert, weil die Signatur pro Zeile etwas mehr Platz braucht.
- Keine Treffer: Servernamen mit Beschreibung auflisten und auf `server` plus leere Query hinweisen. Bei Queries, die wie ein Toolname aussehen, die 3 nächsten Namen per Levenshtein vorschlagen (Executor).
- `describe_tools` und `call_tool` mit unbekanntem Namen: ebenfalls die nächsten Namen vorschlagen statt nur `unknown` (Executor `missing[].matches`). `describe_tools` auf etwa 20 Namen begrenzen (Executor `describePaths: 20`).
- `include_schema` bleibt als Detailstufe. Das entspricht dem „detail level“ aus Anthropics Code-Execution-Artikel.

### P2: Meta-Tool-Beschreibungen

Die Texte sollen den Suchmechanismus nennen (Pi und Codex schreiben ausdrücklich „with BM25“), Beispiele für gute Queries geben und den Arbeitsablauf festlegen. Die Serverliste darf in der Beschreibung bleiben, weil sie aus der Config kommt und statisch ist. Pi entfernt sie nur, weil seine Server-Zusammenfassungen sich beim Connect ändern. Länge pro Server wie Pi kappen (250 Zeichen, 4 096 gesamt). Vorschlag:

`search_tools`:

> Search the tools of the connected MCP servers. Most tools of this gateway are not listed directly; use this tool to find them. Ranks tools with BM25 over tool names, descriptions and parameter names. Use a few keywords for the action and the object, e.g. "create issue", "list pull requests", "read file". An exact tool name ranks that tool first. Returns tool names with their parameters and a one-line description. An empty query with `server` lists all tools of that server. Servers:
> - github: GitHub: Issues, Pull Requests, Repositories, Code-Suche
> - …

- `query`: „Keywords for what you want to do, e.g. 'create issue' or 'search code'. Or an exact tool name.“
- `server`: „Only search the tools of this server.“
- `limit`: „Maximum number of results (default 8).“
- `include_schema`: „Return full input schemas instead of one-line signatures.“

`describe_tools`:

> Get the full definitions (description and input schema) of tools by their exact names from search_tools. Unknown names are reported with the closest matching names.

`call_tool`:

> Run a tool found via search_tools. Pass its exact name and arguments matching its input schema; if you are unsure about the arguments, check them with describe_tools first.

Server-Instructions (`get_info`): wie Claude Code empfiehlt, die Kategorien nennen, nicht nur den Mechanismus. Zum Beispiel: „This gateway connects GitHub, filesystem, … tools. They are not listed directly: find them with `search_tools`, …“ Die Kategorien lassen sich aus den Server-`description`s erzeugen.

### P3: Variante „Tools nachladen“ über `tools/list_changed`

Machbar, aber **nicht als Default**:

- **Für den Ansatz spricht:** Das Modell ruft das Tool nativ auf, mit Schema im Kontext und clientseitiger Validierung. Pi (`tool_search` → `setActiveTools`), Codex, Anthropic (`tool_reference`) und Docker (`mcp-add` → `AddTool` → `list_changed`) machen das so.
- **Prompt-Cache:** Pi, Codex und Anthropic laden *clientseitig*. Anthropic hängt geladene Definitionen inline an, damit der Prefix-Cache erhalten bleibt. Ein Server kann das nicht. Nach `list_changed` ändert der Client die Tool-Definitionen, und bei Claude invalidiert „Modifying tool definitions“ den gesamten Cache. Jede Suche würde dann den Cache verwerfen.
- **Client-Verhalten uneinheitlich:** Claude Code aktualisiert interaktiv die Liste, deferiert die neuen Tools aber wegen der eigenen Tool-Suche sofort wieder (es entsteht wieder eine zweistufige Suche). Im `-p`-Modus und im SDK wird nur die Tool-Liste aktualisiert. Ob und wann andere Clients nach `list_changed` neu laden, ist pro Client zu prüfen. Laut Spec „SHOULD“ der Server die Notification senden. Wie Clients reagieren, regelt die Spec nicht (https://modelcontextprotocol.io/specification/2025-11-25/server/tools#list-changed-notification).
- **Implementierung:** Die geladene Menge muss **pro Session** gehalten werden. Heute verwaltet `Hub` die Peers global und benachrichtigt alle. Für `/mcp` im HTTP-Modus mit mehreren Clients bräuchte es Session-State im `Gateway` statt im `Hub`. Wie Pi sollte das Ergebnis sagen „available from your next call“, und `call_tool` muss als Fallback bleiben.

Empfehlung: höchstens als eigener, opt-in Endpunkt bzw. Modus (z. B. `/mcp/progressive`) für Clients ohne eigene Tool-Suche und ohne Prompt-Caching-Sorgen. Vorher die P1-Punkte umsetzen, die allen Modi nützen.

### P3: Qualitätssicherung

- Ranking-Unit-Tests mit kleinem Korpus: Namenstreffer schlägt Beschreibungstreffer, Abdeckung schlägt Einzeltreffer (Pis Beispiel `screenshot window`), exakter Name zuerst, camelCase/snake_case-Split, Plural.
- Eine kleine Golden-Liste „Query → erwartetes Tool“ aus den realen Upstreams der eigenen Config (Executor hat dafür `e2e/benchmarks/discovery.ts`), als ignorierter Test oder Subcommand `emcipi search "<query>"` analog zu `emcipi tools`.

### Was bewusst nicht empfohlen wird

- **Embeddings bzw. Hybrid** (Docker `find-tools`, ToolHive, VS Code): Sie brauchen ein externes Modell oder einen API-Key und widersprechen dem Ziel „kleines Gateway“. ToolHive und Docker bieten sie nur optional an. Der BM25-Zweig ist der robuste Kern.
- **Regex-Suche** (Anthropic-Regex-Variante, mcpc `-E`): Für Modelle ist das unnatürlicher als Keywords, Anthropic bietet deshalb zusätzlich BM25 an. Ein `+term`-Pflichtfilter wie in Claude Codes `ToolSearch` wäre die günstigere Alternative, falls Bedarf entsteht.
- **Code Mode:** braucht eine Sandbox und ist für emcipi außer Scope.
