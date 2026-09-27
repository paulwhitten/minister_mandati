# Design: Composable System Prompt

Status: draft / proposal
Scope: `src/config.rs`, `src/context.rs` (system prompt assembly)
Related: `design-loop-guards.md` (the default suffix carries the
"do not repeat a successful tool call / terminate cleanly" rules)

## Problem

The system prompt is fixed in `default_system_prompt()`. The only override today
is `[agent].system_prompt_override`, which replaces the whole prompt. There is no
way to add a persona or a few house rules without discarding the built-in
Linux-first expertise.

## Goals

- Compose the system prompt from three segments: prefix, body, suffix.
- Ship sensible defaults so the common path needs no configuration.
- Allow overriding each segment via `agent.toml` or an environment variable.
- Keep a full-replacement escape hatch.
- Remain auditable: an operator can see the exact prompt that was used.

## Segments

| Segment  | Purpose                                   | Default                                              |
|----------|-------------------------------------------|------------------------------------------------------|
| prefix   | Role or persona hook, prepended.          | empty                                                |
| body     | Domain expertise.                         | the current Linux-first expert prompt (see below)    |
| suffix   | Operational and safety rules, appended.   | small verifiable steps; do not repeat a successful tool call; terminate cleanly |

The prompt is the non-empty segments joined by a blank line, trimmed.

Moving the termination guidance into the suffix lets us soften it (a brief
acknowledgment is enough to finish), which directly addresses the loop in
an early development run where "do not tell me what you did" fought a prompt that
demanded a normal closing message.

## Naming conventions

- Config file keys: lower `snake_case`, no prefix (under `[agent]`).
- Environment variables: `MIMA_` prefix, `UPPER_SNAKE_CASE`.

| Config key (`[agent]`)   | Environment variable          |
|--------------------------|-------------------------------|
| `system_prompt_override` | `MIMA_SYSTEM_PROMPT`          |
| `system_prompt_prefix`   | `MIMA_SYSTEM_PROMPT_PREFIX`   |
| `system_prompt_body`     | `MIMA_SYSTEM_PROMPT_BODY`     |
| `system_prompt_suffix`   | `MIMA_SYSTEM_PROMPT_SUFFIX`   |

## Precedence (per segment)

For each segment, resolve in order and stop at the first hit:

1. Environment variable (`MIMA_SYSTEM_PROMPT_*`).
2. `agent.toml` (`[agent].system_prompt_*`).
3. Built-in default.

A value that is present but empty suppresses that segment. This distinguishes
"unset" (use the default) from "intentionally empty" (omit the segment):

- Environment: `std::env::var` returns `Ok("")` for set-but-empty and `Err` for
  unset.
- Config: the three segment fields are `Option<String>`, so `None` means absent
  and `Some("")` means intentionally empty.

## Full-replacement override

`system_prompt_override` / `MIMA_SYSTEM_PROMPT`, when set to a non-empty value,
bypasses composition entirely:

1. `MIMA_SYSTEM_PROMPT` (env), if non-empty.
2. `[agent].system_prompt_override` (config), if non-empty.
3. Otherwise compose from prefix, body, suffix.

## Config schema

```toml
[agent]
# Full replacement; when non-empty it bypasses composition.
system_prompt_override = ""

# Composed segments. Omit a key to use its built-in default; set it to ""
# to suppress that segment.
# system_prompt_prefix = "You are the on-call kernel maintainer."
# system_prompt_body   = "..."
# system_prompt_suffix = "..."
```

Config struct additions (`AgentConfig`):

```rust
#[serde(default)]
system_prompt_override: String,
#[serde(default)]
system_prompt_prefix: Option<String>,
#[serde(default)]
system_prompt_body: Option<String>,
#[serde(default)]
system_prompt_suffix: Option<String>,
```

## Resolution algorithm

```rust
fn resolve_segment(env_key: &str, toml_val: &Option<String>, default: &str) -> String {
    if let Ok(v) = std::env::var(env_key) {
        return v; // set, possibly empty -> wins over config and default
    }
    if let Some(v) = toml_val {
        return v.clone(); // present, possibly empty
    }
    default.to_string()
}

fn compose_system_prompt(cfg: &AgentConfig) -> String {
    if let Ok(v) = std::env::var("MIMA_SYSTEM_PROMPT") {
        if !v.is_empty() {
            return v;
        }
    }
    if !cfg.system_prompt_override.is_empty() {
        return cfg.system_prompt_override.clone();
    }

    let prefix = resolve_segment("MIMA_SYSTEM_PROMPT_PREFIX", &cfg.system_prompt_prefix, DEFAULT_PREFIX);
    let body = resolve_segment("MIMA_SYSTEM_PROMPT_BODY", &cfg.system_prompt_body, DEFAULT_BODY);
    let suffix = resolve_segment("MIMA_SYSTEM_PROMPT_SUFFIX", &cfg.system_prompt_suffix, DEFAULT_SUFFIX);

    [prefix, body, suffix]
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}
```

## Integration

- Move prompt assembly behind `Config` (for example `Config::system_prompt()`),
  so `AgentContext::new` calls it instead of choosing override-or-default inline.
- Existing `${ENV_VAR}` expansion already runs over the config file contents at
  load. For parity, apply the same expansion to segment values that come from
  environment variables.

## Default segment contents (proposed)

- `DEFAULT_PREFIX`: empty.
- `DEFAULT_BODY`: the current Linux-first expert paragraph, minus its trailing
  termination sentence.
- `DEFAULT_SUFFIX`:
  "Prefer small, verifiable steps and idiomatic, standard practices. Do not
  repeat a tool call that has already succeeded. When the task is complete, stop
  and request no further tools; a brief acknowledgment is sufficient even if
  asked to be silent."

With no overrides, the composed prompt equals the old body plus this suffix.
That is a small, intentional wording change to the termination guidance; it is
called out here so the behavior shift is explicit.

## Observability

Log the composed prompt once at startup: `tracing::debug!(system_prompt = %p,
"composed system prompt")`. Debug level keeps normal runs quiet while preserving
the audit trail on demand.

## Testing

- `resolve_segment` precedence: env beats config beats default; set-empty
  suppresses.
- Composition joins non-empty segments with a blank line and trims.
- Full override via env and via config each short-circuits composition.
- Default composition equals the expected combined string.

## Open questions

- Resolved: the env namespace is unified under `MIMA_*` (`MIMA_BASE_URL`,
  `MIMA_MODEL`, `MIMA_API_KEY`, `MIMA_LOG`); the old `AGENT_*` names were removed
  outright since the project is pre-release with no external users.
- Should environment-provided segments also receive `${VAR}` expansion?
  Proposed yes, for parity with config-file values.
