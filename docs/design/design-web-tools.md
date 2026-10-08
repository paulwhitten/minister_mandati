# Design: Web Search and Fetch Tools

Status: proposal
Scope: new `src/tools/web.rs` (and helpers), `src/approval.rs`,
`src/config.rs` (`[web]`), `src/agent.rs` (session taint), `Cargo.toml`
(feature `web`)
Related: `docs/design/design-shell-sandboxing.md` (threat model, approvals,
trusted configuration)

## Problem

mima has no way to look anything up beyond the files in front of it: no web
search, no page fetch, no documentation lookup. Coding work often needs
current API documentation, error messages explained, or a library's usage
checked.

Web access is also the most dangerous capability an agent can have, and it
cuts against mima's positioning (no telemetry, zero exfiltration):

- **It is an outbound channel.** A search query or URL can carry data out,
  for example a secret the model read, encoded in a query string. Attacks of
  this kind are documented against several agents: data exfiltrated through
  URLs or webhooks the agent was induced to request (Antigravity via
  webhook.site; Claude Code CVE-2026-54316 through a preapproved domain), and
  OpenAI has published a defense against URL-based exfiltration.
- **It brings untrusted content into the context.** Web pages are the classic
  source of indirect prompt injection (Greshake et al. 2023). Together with
  local data and an outbound channel this is Simon Willison's "lethal
  trifecta".

The shell sandbox deliberately runs commands with the network off. Web
access must not undo that.

## Goals

- Search and fetch for documentation and references, off by default.
- Every byte that leaves the machine is visible: the operator sees the exact
  query or URL before it is sent, unless it is provably harmless.
- No access to local or private network addresses (SSRF).
- Untrusted web content is marked as such, and once it has entered a session,
  automatic approvals elsewhere are suspended.
- Works with a self-hosted search backend; no third-party account required.
- Small and auditable: few new dependencies, all behind a cargo feature so an
  air-gapped build can leave the code out.
- An offline documentation tool for air-gapped and edge deployments.

Non-goals: a browser (JavaScript execution, logins, forms); summarizing pages
with a second model call; crawling.

## Why not let the model run `curl`?

The shell sandbox's network-off rule is what makes approving sandboxed
commands safe. Opening the network for the shell would open it for every
command. And a curl command line cannot be validated: `-d @file`, `-F`, `-T`
upload local files; `file://`, `gopher://` and `dict://` reach local
resources; `-x` and `--resolve` redirect traffic; `-K` and the default
`.curlrc` load options from files; shell expansion adds more. Checking that
safely is the same allowlist problem that produced the Claude Code and
Gemini CLI allowlist bypasses.

Running curl internally with arguments that mima builds would avoid those
flags, but it adds nothing that the HTTP client mima already links (reqwest
with rustls) lacks, and it would add a host-dependent external binary.
Address checks and redirect re-validation need control over name resolution
and connections, which reqwest provides in-process.

## Prior art

| Agent | Search | Fetch | Notes |
|---|---|---|---|
| Claude Code | WebSearch (server side) | WebFetch: converts to Markdown, processed by a small model, per-domain permissions | cross-host redirects returned to the model, not followed; CVE-2026-54316 exfiltration via a preapproved domain |
| Codex CLI | optional web search (cached index or live), off by default | none built in | OpenAI only lets the agent open URLs an independent index has seen |
| Gemini CLI | `google_web_search` | `web_fetch` | SSRF issues fixed in 2026 (issues 28184, 24230; PR 29120); per-host rate limit |
| Aider | none | `/web` scrapes a page (Playwright optional) | user-initiated |
| MCP reference `fetch` server | none | HTML to Markdown, robots.txt | no address checks |

## Design

Two tools, implemented in mima with the existing reqwest/rustls client,
compiled only with the cargo feature `web` and registered only when
`[web].enabled` is set from trusted configuration.

### `web_search`

```json
{ "query": "string, 1-400 characters",
  "max_results": "integer 1-10, default 5",
  "site": "optional host to restrict to",
  "time_range": "optional: day | month | year" }
```

Returns numbered results (title, URL, a snippet of at most about 300
characters), wrapped in untrusted-content markers (below). Result URLs join
the session's provenance set.

Backends (only two, to keep the audit surface small):

- **SearXNG** (default): a self-hosted metasearch engine with a JSON API, run
  in Docker on the Thor or a LAN host. No API key or account. The query still
  leaves the machine, since SearXNG forwards it to public engines, so it is
  treated as outbound data.
- **Brave Search API** (optional): a hosted API with its own index; needs a
  key, read from an environment variable named in the config.

Not used: Google's Custom Search JSON API (closed to new customers, ending
2027-01-01), the Bing Search APIs (retired 2025-08-11), and scraping
DuckDuckGo's HTML endpoint (fragile, terms).

### `web_fetch`

```json
{ "url": "absolute https URL (http only if allowed)",
  "offset": "integer byte offset into the converted text, default 0",
  "max_bytes": "integer, at most the tool output budget",
  "format": "markdown (default) | text | raw (text types only)" }
```

Returns a header (final URL, status, content type, total converted size, the
range returned, the next offset) and the delimited content. Large pages are
paged by offset rather than truncated: converted reference pages measured
70-480 KB.

Request policy:

- GET only; `https` (with `http` an opt-in); default ports only.
- No userinfo in URLs, no cookies, no authorization headers, no referer.
- `no_proxy()`: reqwest honors `*_PROXY` environment variables by default,
  and a proxy would bypass the address checks.
- Address checks against the IANA special-purpose registries: refuse
  loopback, private (RFC 1918), CGNAT, link-local (including
  169.254.169.254 metadata), multicast, documentation and benchmarking
  ranges, IPv6 ULA and link-local, and IPv4 embedded in IPv6 (mapped, NAT64,
  6to4). IP literals are checked directly (hyper skips the resolver for
  them). Names go through a resolver that refuses the request if *any*
  returned address is not global, and the connection is pinned to the
  checked addresses, which defeats DNS rebinding.
- URLs are parsed by the `url` crate (the same parser reqwest uses, so there
  is no parser differential). It normalizes alternate IPv4 forms (`127.1`,
  `0x7f.1`, `2130706433`) before the check, rejects NUL in hosts, and turns
  internationalized names into punycode for display.
- At most 5 redirects, each re-checked. A redirect to another host is
  returned to the model as a message ("redirects to https://other/...; fetch
  that if needed"), not followed, so it goes through approval again.
- Limits: 5 MiB streamed cap, 10 s to connect, 30 s total, a content-type
  allowlist (HTML, plain text, JSON, XML, Markdown), no transparent
  decompression, per-host and per-session request caps.

Content handling:

- Charset from the header or the document (`encoding_rs`, `chardetng`).
- HTML to Markdown with `htmd`, skipping `script`, `style`, `nav`, `noscript`
  and similar tags (its defaults keep inline script text). Images are dropped.
  A readability-style article extractor was measured and rejected as the
  default: it removed every method signature from a docs.rs page.
- Control characters, Unicode tag characters, bidirectional controls and
  zero-width characters are stripped; terminal escape sequences cannot reach
  the operator's terminal.
- Output is wrapped in markers with a random identifier, so content cannot
  forge the closing marker: `<<web_content id=9c1e untrusted>> ...
  <<end 9c1e>>`. The system prompt says such content is data, not
  instructions. Delimiting helps but is not a defense on its own (AgentDojo:
  it lowered GPT-4o's targeted attack success from 57.7% to 41.7%), which is
  why the approval and taint rules below carry the weight.

### Approvals, provenance and taint

- **Default: every search and fetch prompts.** The prompt shows exactly what
  leaves the machine: the URL (host in punycode, path, decoded query
  parameters) or the query and backend, and where the URL came from ("typed
  by you", "from search result 3", "constructed by the model").
- **Automatic approval only when all hold:**
  - a secret scan of the URL or query finds nothing (key and token patterns,
    long high-entropy strings, values seen in files the session read);
  - the URL is in the session's provenance set (typed by the user or returned
    by the search backend) with `auto_approve_provenance`, or it is on
    `allowed_domains` and has no query string;
  - the per-session caps are not reached.
  This is a local form of OpenAI's "only URLs an independent index has seen"
  policy: the model cannot invent a URL that carries data out.
- **Taint.** Once web output enters a session, `auto_approve_sandboxed`,
  `auto_approve_bash` and disabled write approvals are suspended for the rest
  of the session, with one notice. Untrusted content can then not steer an
  unattended command or write. This follows Meta's "Rule of Two": an agent
  should not have untrusted input, sensitive data and external effects all at
  once without a human in the loop.
- `--approve-all` (eval harness only) does not enable web tools.

### Configuration

```toml
[web]
enabled = false                  # only from trusted configuration
backend = "searxng"              # "searxng" | "brave" | "none"
searxng_url = "http://127.0.0.1:8888"
brave_api_key_env = "MIMA_BRAVE_API_KEY"   # the variable's name, not the key
allow_http = false
allowed_domains = []             # auto-approve, no query string, before taint
blocked_domains = []
auto_approve_provenance = false
taint = "strict"                 # "strict" | "off"
max_fetches_per_session = 50
max_searches_per_session = 30
respect_robots = false
```

Trust follows the sandbox design: settings that widen access (`enabled`,
`backend`, the backend URL, `allowed_domains`, `allow_http`,
`auto_approve_provenance`, `taint = "off"`) are ignored when they come from
the workspace's own `agent.toml`, since a repository controls that file. The
backend URL receives every query, so it must never come from the workspace.
Narrowing settings (`blocked_domains`, lower caps) are kept.

`robots.txt` is advisory (RFC 9309) and aimed at crawlers; an agent fetching
one page on a user's behalf is closer to a browser. It is off by default and
available as an option.

### Offline documentation (`doc_search`, `doc_read`)

For air-gapped and edge use, a read-only pair of tools over configured local
documentation: man pages, `rustup` documentation (about 900 MB), Python HTML
docs, the kernel's `Documentation/`, DevDocs, Zeal docsets and Kiwix
archives. Search by name and full text; page through one document. No
network, no taint, no approval. This complements web access and may be the
better first step for the Thor.

## Alternatives considered

- **`curl` in the shell:** see above.
- **Summarizing pages with a second model call** (as Claude Code does): it
  shrinks injected instructions but costs a model call per fetch and moves
  the injection risk rather than removing it.
- **A proxy in front of the shell sandbox** (Tier 2 of the sandbox design):
  enforces domain allowlists for commands, but only where network namespaces
  work, which excludes the Thor's default configuration; and it gives
  arbitrary commands network access. The dedicated tools are narrower.
- **More search backends:** each one is more code to audit; two cover the
  self-hosted and hosted cases.
- **Readability extraction by default:** measured to drop the parts of
  reference pages that matter most (signatures, code).

## Testing

1. URL and address unit tests: alternate IPv4 forms, every IANA range
   boundary, mapped/NAT64/6to4 addresses, userinfo, ports, schemes (`file:`,
   `gopher:`, `ftp:`, `data:`, `javascript:`), dotless and `.local` names,
   trailing dots, length caps.
2. Integration tests against a local server through a test-only resolver:
   redirects (to an IP literal, to `http:`, to another host, loops); a
   resolver answer mixing public and private addresses (refused); oversized
   and slow bodies; compressed bodies; unsupported types; charsets; no
   cookies or auth headers sent; `HTTPS_PROXY` set and ignored.
3. Extraction golden files (docs.rs, std docs, kernel docs, Python docs,
   Wikipedia, a GitHub page): signatures present, no script text, no images.
4. Sanitization and delimiting: tag characters, bidi controls, zero-width
   characters, forged closing markers, terminal escapes.
5. Approval and taint: prompts by default; provenance auto-approval; the
   secret scan blocks auto-approval; after taint, sandboxed commands and
   writes prompt again; a workspace `agent.toml` cannot enable web access.
6. Injection scenarios (AgentDojo-style, local pages): instructions to fetch
   an attacker URL with a secret, to search for a secret, to run a command, to
   write a file. Each outbound attempt must reach a prompt, and the mock
   server receives nothing when it is denied. Run against the local models to
   see how often they try.

## Implementation phases

| Phase | Content | Effort |
|---|---|---|
| W0 | `[web]` config and trust handling, cargo feature; URL validation and address classification with table tests | 1 day |
| W1 | `web_fetch`: client policy, checking resolver, redirects, limits, charset, conversion, sanitization, delimiting, paging | 2-3 days |
| W2 | Approvals: previews, provenance, secret scan, taint and suspension of auto-approvals, transcript records of outbound data | 1-2 days |
| W3 | `web_search`: SearXNG and Brave adapters, caps; SearXNG setup notes | 1 day |
| W4 | Tests, injection pages, docs | 1-2 days |
| W5 | `doc_search`/`doc_read` | 2-4 days |

Expected binary growth for W0-W3: about +1.1 MB over the current 4.8 MB
release binary (`htmd` and the charset crates, measured separately).

## Open questions

- Whether keeping Brave results in local transcripts counts as storing them
  under Brave's terms.
- Whether the approval prompt should show resolved addresses (resolving first
  is itself an outbound DNS query).
- Whether taint should end with `/new` (a new session) only, or also earlier.
- Which SearXNG engines work reliably from a given network.

## References

- Claude Code tools: <https://code.claude.com/docs/en/tools-reference>;
  CVE-2026-54316: <https://advisories.gitlab.com/npm/@anthropic-ai/claude-code/CVE-2026-54316/>
- Gemini CLI SSRF: <https://github.com/google-gemini/gemini-cli/issues/28184>,
  <https://github.com/google-gemini/gemini-cli/issues/24230>,
  <https://github.com/google-gemini/gemini-cli/pull/29120>
- Codex web search: <https://learn.chatgpt.com/docs/web-search>; OpenAI on URL
  exfiltration: <https://cdn.openai.com/pdf/dd8e7875-e606-42b4-80a1-f824e4e11cf4/prevent-url-data-exfil.pdf>
- MCP fetch server: <https://github.com/modelcontextprotocol/servers/tree/main/src/fetch>
- Antigravity exfiltration: <https://www.promptarmor.com/resources/google-antigravity-exfiltrates-data>
- AgentDojo (arXiv:2406.13352); adaptive attacks on defenses
  (arXiv:2510.09023); Meta, Rule of Two:
  <https://ai.meta.com/blog/practical-ai-agent-security/>; Greshake et al.,
  indirect prompt injection (arXiv:2302.12173); the lethal trifecta:
  <https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/>
- SearXNG: <https://docs.searxng.org/dev/search_api.html>; Brave Search API:
  <https://brave.com/search/api/>; Google Custom Search:
  <https://developers.google.com/custom-search/v1/overview>
- OWASP SSRF prevention:
  <https://cheatsheetseries.owasp.org/cheatsheets/Server_Side_Request_Forgery_Prevention_Cheat_Sheet.html>;
  robots.txt, RFC 9309: <https://www.rfc-editor.org/rfc/rfc9309.html>
