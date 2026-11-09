# emcipi

Schlankes MCP-Gateway in Rust für Linux, macOS und Windows. Bündelt beliebig viele MCP-Server (stdio oder Streamable HTTP) hinter einem einzigen Endpunkt und wird über eine TOML-Datei konfiguriert.

- **Upstreams:** stdio (Kindprozess) und Streamable HTTP, inklusive eigener Header
- **Downstream:** `emcipi stdio` oder `emcipi http`
- **Modi:** `static` (alle Tools direkt, Default) oder `dynamic` (die KI sucht Tools über Meta-Tools), per CLI, Config, pro Server oder per HTTP-Endpunkt
- **Secrets von außen:** Platzhalter für Umgebungsvariablen, Dateien und Kommandos (Secret Manager)
- **Idle-Stopp:** ungenutzte stdio-Server werden beendet und bei Bedarf neu gestartet
- **Tool-Filter:** `include_tools` und `exclude_tools` mit `*`-Globs, gelten auch für Aufrufe
- **Ressourcenbedarf:** ca. 8 MB RSS (ohne die Upstream-Prozesse selbst)

## Installation

Fertige Binaries für Linux (x86_64, ARM64), macOS (Apple Silicon, Intel) und Windows (x86_64) gibt es unter [Releases](../../releases), jeweils mit `README.md` und `config.example.toml`. Prüfsummen stehen in `SHA256SUMS`.

```sh
tar xzf emcipi-v0.1.0-aarch64-apple-darwin.tar.gz
cp emcipi-v0.1.0-aarch64-apple-darwin/emcipi ~/.local/bin/
```

Die macOS-Binaries sind nicht signiert. Wurde das Archiv über den Browser geladen, blockiert Gatekeeper den ersten Start. Abhilfe: `xattr -d com.apple.quarantine ~/.local/bin/emcipi`. Bei Downloads mit `curl` oder `gh release download` tritt das nicht auf.

Selbst bauen:

```sh
cargo build --release
cp target/release/emcipi ~/.local/bin/        # Windows: target\release\emcipi.exe
```

Unter Windows braucht der Build kein NASM, weil `.cargo/config.toml` die vorgebauten Objekte der TLS-Bibliothek aktiviert.

### Release erstellen

Version in `Cargo.toml` erhöhen, committen, dann taggen:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

`.github/workflows/release.yml` prüft, ob der Tag zur Version in `Cargo.toml` passt. Danach testet und baut es auf allen Plattformen und veröffentlicht das GitHub-Release mit Archiven und Prüfsummen.

## Benutzung

```sh
emcipi tools                       # alle Server verbinden, Tools auflisten (zum Testen der Config)
emcipi search "create issue"       # Tool-Suche testen, Ausgabe wie für das Modell
emcipi http                        # Streamable HTTP auf http.listen (Standard 127.0.0.1:8808/mcp)
emcipi stdio                       # für Clients, die einen Prozess starten wollen
emcipi --mode dynamic stdio        # Standardmodus überschreiben
emcipi -c ./config.toml --idle-timeout 300 http --listen 0.0.0.0:9000
```

| Flag | Umgebungsvariable | Wirkung |
|---|---|---|
| `-c, --config` | `EMCIPI_CONFIG` | Pfad zur Config |
| `-m, --mode static\|dynamic` | `EMCIPI_MODE` | Standardmodus für Server ohne eigenes `mode` |
| `--idle-timeout SECS` | `EMCIPI_IDLE_TIMEOUT` | globales `idle_timeout_secs`, 0 = nie |
| `http --listen ADDR` | `EMCIPI_LISTEN` | `http.listen` |
| `http --auth-token T` | `EMCIPI_AUTH_TOKEN` | `http.auth_token`; lieber per Env setzen, Flags sind in `ps` sichtbar |

Logs gehen nach stderr, das Level steuerst du über `RUST_LOG` (Default `info,rmcp=warn`).

### stdio oder HTTP?

Im **HTTP-Modus** läuft ein einziger Gateway-Prozess mit einem Satz Upstream-Prozessen, den sich alle Clients teilen. Das spart den meisten RAM. Im **stdio-Modus** startet jeder Client sein eigenes Gateway mitsamt eigenen Upstreams.

Der HTTP-Server bietet drei Endpunkte auf denselben Upstreams:

| Endpunkt | Standardmodus |
|---|---|
| `/mcp` | aus Config bzw. `--mode` (Default `static`) |
| `/mcp/static` | `static` |
| `/mcp/dynamic` | `dynamic` |

Clients mit eigener Tool-Suche (Claude Code, Pi) nutzen `/mcp/static`, Clients ohne eigene Suche `/mcp/dynamic`. Die Server laufen dabei trotzdem nur einmal. Ein `mode` direkt am Server hat immer Vorrang.

```sh
claude mcp add -s user --transport http emcipi http://127.0.0.1:8808/mcp/static \
  --header "Authorization: Bearer $EMCIPI_AUTH_TOKEN"
pi mcp add emcipi --url http://127.0.0.1:8808/mcp/static --bearer-token-env-var EMCIPI_AUTH_TOKEN
```

## Konfiguration

Siehe [`config.example.toml`](config.example.toml).

### Secrets

Secrets gehören nicht in die Config. In `command`, `args`, `env`, `cwd`, `url`, `headers` und `http.auth_token` werden Platzhalter aufgelöst:

| Platzhalter | Quelle |
|---|---|
| `${VAR}`, `${env:VAR}` | Umgebungsvariable, Fehler wenn nicht gesetzt |
| `${VAR:-default}` | mit Fallback |
| `${file:pfad}` | Dateiinhalt; `~/` und Pfade relativ zur Config-Datei |
| `${cmd:programm args}` | stdout des Programms, z. B. `op read …`, `gh auth token`, `security find-generic-password …`, `pass show …` |
| `$${` | literales `${` |

- Aufgelöst wird **bei jedem Verbindungsaufbau**, nicht beim Start. Rotierende Tokens funktionieren dadurch, und ein Neustart nach Idle-Stopp holt das Secret frisch.
- Die Syntax wird schon beim Start geprüft.
- Kommandos laufen mit 30 s Timeout im Verzeichnis der Config. Schlägt ein Kommando fehl, schlägt der Connect fehl.
- `http.auth_token` wird einmalig beim Start aufgelöst. Ein leerer Wert ist ein Fehler.
- **Umgebung der Server:** Ein stdio-Server sieht standardmäßig nur eine Basis-Umgebung plus sein eigenes `env`, also keine Secrets aus der Umgebung des Gateways. Was er braucht, übergibst du explizit, z. B. `env = { TOKEN = "${TOKEN}" }`. `inherit_env = true` gibt ihm die komplette Umgebung. Zur Basis-Umgebung gehören:
  - **Linux/macOS:** `PATH`, `HOME`, `USER`, `SHELL`, `TMPDIR`, `TZ`, Locale (`LANG`, `LC_*`) und die XDG-Verzeichnisse
  - **Windows:** `PATH`, `PATHEXT`, `SystemRoot`, `APPDATA`, `LOCALAPPDATA`, `TEMP`, `USERPROFILE` u. a., ohne die viele Programme (auch Node.js) nicht starten
  - **überall:** Proxy- und Zertifikats-Einstellungen (`HTTP(S)_PROXY`, `NO_PROXY`, `SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`, …), damit `npx`/`uvx` auch hinter einem Firmen-Proxy Pakete laden können

#### Kommandos

`${cmd:…}` startet das Programm **direkt, ohne Shell**. Dadurch verhält sich derselbe Eintrag auf Linux, macOS und Windows gleich, unabhängig von der Login-Shell. Den Befehl zerlegt emcipi so in Wörter:

- Leerzeichen trennen die Wörter.
- `'…'` wird wörtlich übernommen.
- In `"…"` sind nur `\"` und `\\` Escapes.
- Ein Backslash außerhalb von Anführungszeichen ist ein normales Zeichen, Windows-Pfade wie `C:\Tools\op.exe` funktionieren also ohne Quoting.

```toml
env = { GITHUB_TOKEN = "${cmd:gh auth token}" }
headers = { Authorization = "Bearer ${cmd:op read \"op://Private/My Item/token\"}" }
```

Pipes, Umleitungen und Variablen gibt es dabei nicht. Wer sie braucht, ruft die Shell explizit auf. Das ist dann bewusst plattformspezifisch:

```toml
env = { X = "${cmd:sh -c 'pass show x | head -1'}" }                          # Linux/macOS
env = { X = "${cmd:pwsh -NoProfile -Command Get-Secret x -AsPlainText}" }     # Windows
```

Programme werden über `PATH` gesucht, unter Windows inklusive `PATHEXT`, also findet emcipi z. B. `npx.cmd`. Das gilt auch für `command` von stdio-Servern. Setzt ein Server in seinem `env` ein eigenes `PATH`, wird sein `command` dort gesucht. Kommandos in `${cmd:…}` sucht emcipi dagegen immer im eigenen `PATH`.

**macOS:** Starten Claude Desktop oder launchd emcipi, fehlt im `PATH` meist `/opt/homebrew/bin`. Dann schlagen `${cmd:op read …}` oder `command = "npx"` mit „not found in PATH“ fehl. Abhilfe: absolute Pfade (`/opt/homebrew/bin/op`) oder `PATH` in der Umgebung setzen, mit der emcipi gestartet wird (z. B. `env` in der Claude-Desktop-Config oder `EnvironmentVariables` in der launchd-Plist).

`${cmd:…}` führt beliebige Programme aus. Die Config ist damit so vertrauenswürdig wie ein Skript und sollte nur für dich schreibbar sein.

## Dynamischer Modus

Ein Server im Modus `dynamic` taucht nicht direkt in `tools/list` auf. Stattdessen bekommt der Client drei Meta-Tools:

| Tool | Zweck |
|---|---|
| `search_tools` | Stichwortsuche (`query`, optional `server`, `limit`, `include_schema`). Liefert pro Treffer eine Signatur wie `github__create_issue(owner, repo, title, body?)` plus die erste Zeile der Beschreibung. Eine leere Query mit `server` listet alle Tools dieses Servers, eine leere Query ohne `server` die Server. |
| `describe_tools` | Vollständige Beschreibung und Input-Schema für bis zu 20 Tool-Namen. |
| `call_tool` | Ruft ein Tool auf (`name`, `arguments`). |

Bei unbekannten Namen schlagen alle drei Meta-Tools die ähnlichsten Tool-Namen vor. Ein Tippfehler startet dabei keinen gestoppten Server, weil die Tool-Liste aus dem Cache kommt.

### Ranking

Die Suche (`src/search.rs`) orientiert sich an Pi. Hintergrund und Vergleich mit anderen Tools stehen in [`docs/research/tool-search.md`](docs/research/tool-search.md).

- **Verfahren:** gefeldetes BM25 (k1 = 1.2, b = 0.75)
- **Gewichtung:**
  - Treffer im Tool-Namen zählen 3 × IDF und schlagen damit beliebig viele Treffer in Beschreibungen.
  - Beschreibung, Titel und Parameter-Namen/-Beschreibungen des Input-Schemas (rekursiv, inklusive `$ref`) laufen über BM25 mit Längennormalisierung.
  - Servername, `description` und Server-Instructions zählen 0,5 × IDF. Sie verstärken einen Treffer, wählen aber allein kein Tool aus.
- **Sortierung:** zuerst exakter Name, dann Anzahl der abgedeckten Suchbegriffe, dann Score. Standardmäßig 8 Treffer, maximal 50.
- **Tokenisierung:**
  - zerlegt camelCase, snake_case und kebab-case
  - entfernt Stoppwörter
  - reduziert Plurale (auch `PRs` → `pr`)
  - Suchbegriffe ohne exakten Treffer zählen ab 4 Zeichen als Präfix mit halbem Gewicht (`config` → `configuration`).

## Lebenszyklus der Upstreams

- Beim Start verbindet das Gateway alle Server parallel im Hintergrund und lernt ihre Tools. Ein ausgefallener Server blockiert die anderen nicht. Neue Versuche gibt es frühestens nach 30 s.
- Damit hinter `npx`/`uvx` keine `node`- oder `python`-Prozesse übrig bleiben, wird immer der ganze Prozessbaum beendet:
  - **Linux/macOS:** Der Server läuft in einer eigenen Prozessgruppe. Beim Stoppen wird stdin geschlossen, nach 3 s bekommt die ganze Gruppe SIGTERM und nach weiteren 3 s SIGKILL.
  - **Windows:** Der Server läuft in einem Job Object. Nach dem Schließen von stdin und 3 s Wartezeit wird der ganze Job beendet. Das passiert auch, wenn emcipi selbst abstürzt.
- Mit `idle_timeout_secs` wird ein stdio-Server beendet, wenn er so lange keinen Aufruf hatte und nichts mehr läuft. Die Tool-Liste bleibt im Cache: `tools/list` und `search_tools` starten ihn nicht neu, erst ein echter Tool-Aufruf. HTTP-Upstreams sind davon nicht betroffen.
- Bricht eine Verbindung ab, verbindet sich das Gateway beim nächsten Zugriff neu.
- Meldet ein Upstream `notifications/tools/list_changed`, wird der Cache verworfen und die Benachrichtigung an die Clients weitergeleitet.
- Fehler von Upstreams kommen als `isError`-Tool-Result zurück, damit das Modell sie lesen kann.

### Wann ist Idle-Stopp riskant?

MCP ist **nicht** zustandslos: Jede Verbindung hat eine Session mit Handshake, und viele Server halten Zustand im Speicher. Ein Neustart verliert diesen Zustand. Unkritisch sind Server, die pro Aufruf eine API ansprechen (GitHub, Linear, Context7, Dateisystem). Kritisch sind Server mit Sitzungszustand:

- Browser-Automation (Playwright, Puppeteer, Chrome DevTools): offene Tabs und Logins sind weg
- In-Memory-Speicher (z. B. `server-memory` ohne Datei, `sequential-thinking`)
- offene DB-Transaktionen, Shell- oder REPL-Sessions, laufende Hintergrundjobs
- Resource-Subscriptions

Für solche Server `idle_timeout_secs = 0` setzen.

## Nicht enthalten (bisher)

Resources, Prompts, Sampling/Elicitation-Weiterleitung, OAuth für Upstreams (nur Header), Hot-Reload der Config, persistenter Tool-Cache über Gateway-Neustarts.
