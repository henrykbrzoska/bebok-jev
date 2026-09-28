# bebok-jev

Jev smart permission auto-approver for Bebok. Before a permission popup is
shown to the user, the engine sends the pending tool call to this plugin's
`decide` action and auto-applies the verdict when Jev is confident.

Jev (`typesafe/jev-1.13`, via the OpenRouter Decisions API) is a structured
decision model: it takes typed questions about a given state and returns
typed answers with calibrated probabilities — no free-form text to parse.

## Setup

1. Install this plugin (Settings → General → Plugins → Install).
2. Put an OpenRouter API key in the environment where the Bebok server runs:
   `export OPENROUTER_API_KEY=sk-or-v1-…` (or set `JEV_API_KEY_ENV` to use a
   differently-named variable).
3. Enable it: `export JEV_ENABLED=1` before starting the server.

Without `JEV_ENABLED=1` and a key, every `decide` returns
`{"decision": null}` and permission popups behave exactly as before —
the plugin never blocks the user.

## Tuning (all optional, env vars)

- `JEV_APPROVE_THRESHOLD` (default `0.85`): `auto_approve` noul at/above →
  `allow`.
- `JEV_DENY_THRESHOLD` (default `0.15`): noul at/below → `deny`.
- Between the thresholds → `null` (show the popup).
- `JEV_ENDPOINT`, `JEV_MODEL`, `JEV_TIMEOUT_MS`, `JEV_MAX_CONTEXT_CHARS`.

## Cost/latency

Roughly ~240 ms and ~$0.00002 per permission popup — the model is tiny and
output tokens are free.
