# Cloud providers (Azure OpenAI)

How to run `mima` and `mima-eval` against a cloud, OpenAI-compatible endpoint
such as Azure OpenAI. Local vLLM, Ollama and llama.cpp work as before; nothing
here changes their defaults.

Sending a conversation to a cloud endpoint sends the code and files the agent
reads to that provider. mima's default is a local model; a cloud endpoint is
an explicit choice in the configuration.

## Configuration

```toml
[provider]
type = "custom"
api_key = "${MIMA_API_KEY}"   # export the key; do not write it into the file
base_url = "https://<resource>.openai.azure.com/openai/v1"
default_model = "gpt-5.4-mini" # on Azure, the deployment name

[agent]
temperature = "server"         # send no temperature (see below)
max_tokens = 8192              # reasoning tokens count against this
max_tokens_param = "max_completion_tokens"
extra_body = { reasoning_effort = "medium" }

[context]
window = 32768                 # cloud APIs do not report it; see below
```

- **`base_url`** is the API root. mima appends `/chat/completions`, so a URL
  with another API path or a query string (for example the Responses API,
  `.../openai/responses?api-version=...`) gives HTTP 404.
- **`max_tokens_param`** names the request field for the reply limit.
  Newer OpenAI and Azure OpenAI models reject `max_tokens` (HTTP 400,
  "Use 'max_completion_tokens' instead"); vLLM accepts both; Ollama is
  unverified, so the default stays `"max_tokens"`. Any other value fails
  when the config is loaded.
- **`temperature = "server"`** leaves temperature out of requests. With
  reasoning enabled, gpt-5.4-mini accepts only its default (1); 0.2 gives
  HTTP 400. Without reasoning it accepts 0.2.
- **`reasoning_effort`** (passed through `extra_body`): without it,
  gpt-5.4-mini answered with no reasoning tokens in our tests; with
  `"medium"` it reasoned. Reasoning is hidden in the Chat Completions API:
  it costs reply tokens (hence the larger `max_tokens`) but is not returned,
  so it takes no room in the context window and `[agent].keep_reasoning`
  has nothing to keep.
- **`[context].window`**: `GET /models` lists the deployment but no
  `max_model_len`, so mima falls back to 32,768 tokens with a warning. Set
  the window explicitly: the model's documented window to use it fully, or
  the local models' window for like-for-like comparisons.
- **Key**: `MIMA_API_KEY` overrides the file. A literal key in a workspace
  `./agent.toml` can be read by the agent's own tools (and so sent to the
  model and written to transcripts); prefer the environment variable or
  `~/.config/minister_mandati/agent.toml`. Shell commands the agent runs do
  not receive `MIMA_API_KEY`.

## Verified against Azure `/openai/v1`, deployment `gpt-5.4-mini`

| Request                                              | Result                               |
|------------------------------------------------------|--------------------------------------|
| `Authorization: Bearer <key>`                        | Works                                |
| `max_tokens`                                         | HTTP 400                             |
| `max_completion_tokens`                              | Works                                |
| `max_completion_tokens = 1`                          | HTTP 400 (reply cannot finish)       |
| `temperature = 0.2`, no reasoning                    | Works                                |
| `temperature = 0.2` with `reasoning_effort`          | HTTP 400 (only the default, 1)       |
| `reasoning_effort = "medium"`                        | Works; reasoning tokens in `usage`   |
| Native `tools` with reasoning                        | Works                                |
| `stream` with `stream_options.include_usage`         | Works                                |
| `GET /models`                                        | Lists the deployment; no window      |
| `GET /openai/health`                                 | HTTP 404 (harness treats as healthy) |
| `/tokenize`                                          | Not available; estimates are used    |

## Evaluations

`mima-eval` profiles take the same settings in their `mima` table, and the
harness's own requests (readiness, tool-call preflight, memorization probe)
follow `max_tokens_param` and `temperature = "server"` from it. The
readiness check asks for 16 tokens, not 1, because Azure rejects a reply it
cannot finish. See "Cloud endpoints" in `docs/eval.md` for a profile.

Run with the key in the environment, for example
`MIMA_API_KEY=... mima-eval run <suite> --profiles profiles.toml --profile gpt54mini`.
The key is passed to each trial's mima in its environment and is never
written to `run.json` or the trial configs.

## Switching between local and cloud

| Setting                    | Local (vLLM or Ollama) | Azure OpenAI                                    |
|----------------------------|------------------------|-------------------------------------------------|
| `[provider].base_url`      | `http://host:8000/v1`  | `https://<resource>.openai.azure.com/openai/v1` |
| `[provider].default_model` | Served model name      | Deployment name                                 |
| `[provider].api_key`       | Any stub value         | `${MIMA_API_KEY}`                               |
| `[agent].max_tokens_param` | Omit (`max_tokens`)    | `"max_completion_tokens"`                       |
| `[agent].temperature`      | Model's recommendation | `"server"` when reasoning is on                 |
| `[context].window`         | Discovered from vLLM   | Set manually                                    |

## Known gaps

- The Responses API (which can return reasoning for reuse across calls) is
  not supported; mima uses Chat Completions only.
- No startup notice yet when `base_url` points outside the local network.
