//! `bebok-jev` standalone plugin server.
//!
//! Spawned by the Bebok engine from the plugin slot directory and spoken to
//! over **JSON-lines on stdio**: one request object per stdin line, exactly
//! one response object per stdout line. Nothing else may ever be printed on
//! stdout (logs go to stderr, which the engine discards).
//!
//! Supported actions:
//! - `{"action":"decide","tool":"...","input":{...},"pattern":"...",
//!    "agent":"...","directory":"...","decision":null}`
//!   → `{"ok":true,"decision":"allow"|"deny"|null}` — Jev's verdict on the
//!   pending permission popup (`null` = uncertain, show the popup).
//! - `{"action":"status",...}` → `{"ok":true,"status":"ready"|"disabled",...}`.
//!
//! Configuration comes from the environment (the engine spawns us with its
//! own env, so the user's shell exports are visible):
//! - `OPENROUTER_API_KEY` (or the var named in `JEV_API_KEY_ENV`) — required.
//!   Missing/empty key → every `decide` returns `{"decision": null}` (the
//!   engine falls through to the user popup). The key is never logged.
//! - `JEV_ENABLED` (`1`/`true` = on; anything else = off). Off → the plugin
//!   reports `disabled` and every `decide` returns `null` (zero API I/O).
//! - `JEV_ENDPOINT` (default `https://openrouter.ai/api/alpha/decisions`),
//!   `JEV_MODEL` (default `typesafe/jev-1.13`),
//!   `JEV_APPROVE_THRESHOLD` (default `0.85`), `JEV_DENY_THRESHOLD`
//!   (default `0.15`), `JEV_TIMEOUT_MS` (default `3000`, clamped
//!   `100..=30_000`), `JEV_MAX_CONTEXT_CHARS` (default `8000`, clamped
//!   `256..=100_000`).

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Crate version, also reported via `--version`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default Decisions API endpoint.
const DEFAULT_ENDPOINT: &str = "https://openrouter.ai/api/alpha/decisions";
/// Default Jev model id.
const DEFAULT_MODEL: &str = "typesafe/jev-1.13";
/// Default env var holding the API key.
const DEFAULT_API_KEY_ENV: &str = "OPENROUTER_API_KEY";
/// Default `auto_approve` noul at/above which we answer `allow`.
const DEFAULT_APPROVE_THRESHOLD: f64 = 0.85;
/// Default `auto_approve` noul at/below which we answer `deny`.
const DEFAULT_DENY_THRESHOLD: f64 = 0.15;
/// Default per-call HTTP timeout (ms).
const DEFAULT_TIMEOUT_MS: u64 = 3000;
/// Default max chars of serialized tool input sent as state.
const DEFAULT_MAX_CONTEXT_CHARS: usize = 8000;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

struct Config {
    enabled: bool,
    endpoint: String,
    model: String,
    api_key: Option<String>,
    approve_threshold: f64,
    deny_threshold: f64,
    timeout_ms: u64,
    max_context_chars: usize,
}

fn env_bool(name: &str) -> bool {
    matches!(
        std::env::var(name)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn load_config() -> Config {
    let key_env = std::env::var("JEV_API_KEY_ENV")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_string());
    let api_key: Option<String> = std::env::var(&key_env)
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty());
    Config {
        enabled: env_bool("JEV_ENABLED"),
        endpoint: std::env::var("JEV_ENDPOINT")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string()),
        model: std::env::var("JEV_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        api_key,
        approve_threshold: env_f64("JEV_APPROVE_THRESHOLD", DEFAULT_APPROVE_THRESHOLD)
            .clamp(0.0, 1.0),
        deny_threshold: env_f64("JEV_DENY_THRESHOLD", DEFAULT_DENY_THRESHOLD).clamp(0.0, 1.0),
        timeout_ms: env_u64("JEV_TIMEOUT_MS", DEFAULT_TIMEOUT_MS).clamp(100, 30_000),
        max_context_chars: (env_u64("JEV_MAX_CONTEXT_CHARS", DEFAULT_MAX_CONTEXT_CHARS as u64)
            as usize)
            .clamp(256, 100_000),
    }
}

// ---------------------------------------------------------------------------
// Decisions API
// ---------------------------------------------------------------------------

/// Build the Decisions API request body: the tool-call state plus the
/// `auto_approve` (noul) and `risk_level` (choice) questions.
fn build_request_body(
    cfg: &Config,
    tool: &str,
    input: &Value,
    pattern: &str,
    agent: &str,
    directory: &str,
) -> Value {
    let mut args = serde_json::to_string_pretty(input).unwrap_or_else(|_| "{}".to_string());
    if args.chars().count() > cfg.max_context_chars {
        args = args.chars().take(cfg.max_context_chars).collect();
    }
    json!({
        "model": cfg.model,
        "state": {
            "tool": tool,
            "input_args": args,
            "agent": agent,
            "directory": directory,
            "pattern": pattern,
        },
        "questions": {
            "auto_approve": {
                "type": "noul",
                "question": "Given the project context and the agent's current task, is it safe to auto-approve this tool call without asking the user?",
            },
            "risk_level": {
                "type": "choice",
                "question": "How risky is this tool call?",
                "options": ["safe", "caution", "dangerous"],
            },
        },
    })
}

/// Map a calibrated `auto_approve` probability onto a decision:
/// at/above the approve threshold → allow, at/below the deny threshold →
/// deny, in between (or missing/out of range) → `None` (show the popup).
fn decide(auto_approve: Option<f64>, cfg: &Config) -> Option<&'static str> {
    let p = auto_approve?;
    if !(0.0..=1.0).contains(&p) {
        return None;
    }
    if p >= cfg.approve_threshold {
        Some("allow")
    } else if p <= cfg.deny_threshold {
        Some("deny")
    } else {
        None
    }
}

/// Pull a noul probability for `question` out of a Decisions API response.
/// The exact envelope may evolve, so several shapes are accepted; anything
/// unrecognised yields `None` (fall through to the popup).
fn extract_noul(value: &Value, question: &str) -> Option<f64> {
    // Direct probability: `{"auto_approve": 0.9}`.
    if let Some(p) = value.get(question).and_then(|v| v.as_f64()) {
        return Some(p);
    }
    let answer = value.get("answers")?.get(question)?;
    for key in ["probability", "p", "yes", "score", "confidence"] {
        if let Some(p) = answer.get(key).and_then(|v| v.as_f64()) {
            return Some(p);
        }
    }
    // Nested once more: `{"answers": {"auto_approve": {"noul": {"probability": …}}}}`.
    for nested in answer.get("noul").into_iter().chain(answer.get("result")) {
        for key in ["probability", "p", "yes", "score", "confidence"] {
            if let Some(p) = nested.get(key).and_then(|v| v.as_f64()) {
                return Some(p);
            }
        }
    }
    None
}

/// Query the Decisions API for one permission ask. Returns the override
/// decision (`"allow"`/`"deny"`) or `None` when uncertain / on any error.
async fn query_jev(
    client: &reqwest::Client,
    cfg: &Config,
    tool: &str,
    input: &Value,
    pattern: &str,
    agent: &str,
    directory: &str,
) -> Option<&'static str> {
    let key = cfg.api_key.as_deref()?;
    let body = build_request_body(cfg, tool, input, pattern, agent, directory);
    let response = client
        .post(&cfg.endpoint)
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        eprintln!("bebok-jev: API returned {}", response.status());
        return None;
    }
    let value: Value = response.json().await.ok()?;
    let auto = extract_noul(&value, "auto_approve");
    eprintln!("bebok-jev: tool={tool} agent={agent} auto_approve={auto:?}");
    decide(auto, cfg)
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

fn err(msg: impl Into<String>) -> Value {
    json!({ "ok": false, "error": msg.into() })
}

async fn handle(client: &reqwest::Client, cfg: &Config, req: &Value) -> Value {
    let Some(obj) = req.as_object() else {
        return err("request must be a JSON object");
    };
    let action = obj.get("action").and_then(|v| v.as_str()).unwrap_or("");
    match action {
        "status" => json!({
            "ok": true,
            "status": if cfg.enabled && cfg.api_key.is_some() { "ready" } else { "disabled" },
            "model": cfg.model,
        }),
        "decide" => {
            // Graceful degradation: anything missing means "show the popup".
            if !cfg.enabled || cfg.api_key.is_none() {
                return json!({ "ok": true, "decision": Value::Null });
            }
            let tool = obj.get("tool").and_then(|v| v.as_str()).unwrap_or("");
            if tool.is_empty() {
                return err("tool must not be empty");
            }
            let input = obj.get("input").cloned().unwrap_or(Value::Null);
            let pattern = obj.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let agent = obj.get("agent").and_then(|v| v.as_str()).unwrap_or("");
            let directory = obj.get("directory").and_then(|v| v.as_str()).unwrap_or("");
            let decision = query_jev(client, cfg, tool, &input, pattern, agent, directory)
                .await
                .map(|s: &str| Value::String(s.to_string()))
                .unwrap_or(Value::Null);
            json!({ "ok": true, "decision": decision })
        }
        other => err(format!("unknown action '{other}'")),
    }
}

/// Parse one stdin line and produce the response value. Never panics.
async fn respond_to_line(client: &reqwest::Client, cfg: &Config, line: &str) -> Value {
    match serde_json::from_str::<Value>(line) {
        Ok(req) => handle(client, cfg, &req).await,
        Err(e) => err(format!("invalid request: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("bebok-jev {VERSION}");
        return;
    }
    if !args.iter().any(|a| a == "--plugin-server") {
        eprintln!("usage: bebok-jev --plugin-server");
        eprintln!("  Runs the Bebok plugin server (JSON-lines over stdio).");
        std::process::exit(2);
    }

    let cfg = load_config();
    if !cfg.enabled {
        eprintln!(
            "bebok-jev: JEV_ENABLED is not set — every decide returns null (popups unchanged)"
        );
    } else if cfg.api_key.is_none() {
        eprintln!(
            "bebok-jev: enabled but no API key found — every decide returns null (popups unchanged)"
        );
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(cfg.timeout_ms))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let mut reader = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await.unwrap_or(0);
        if n == 0 {
            break; // EOF — the engine went away.
        }
        if line.trim().is_empty() {
            continue;
        }
        let resp = respond_to_line(&client, &cfg, line.trim()).await;
        let mut out = serde_json::to_string(&resp).unwrap_or_else(|_| {
            r#"{"ok":false,"error":"response serialisation failed"}"#.to_string()
        });
        out.push('\n');
        if stdout.write_all(out.as_bytes()).await.is_err() {
            break;
        }
        if stdout.flush().await.is_err() {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            enabled: true,
            endpoint: DEFAULT_ENDPOINT.to_string(),
            model: DEFAULT_MODEL.to_string(),
            api_key: Some("test-key".to_string()),
            approve_threshold: DEFAULT_APPROVE_THRESHOLD,
            deny_threshold: DEFAULT_DENY_THRESHOLD,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_context_chars: DEFAULT_MAX_CONTEXT_CHARS,
        }
    }

    #[test]
    fn decide_respects_thresholds() {
        let cfg = test_config();
        assert_eq!(decide(Some(0.95), &cfg), Some("allow"));
        assert_eq!(decide(Some(0.85), &cfg), Some("allow"));
        assert_eq!(decide(Some(0.05), &cfg), Some("deny"));
        assert_eq!(decide(Some(0.15), &cfg), Some("deny"));
        assert_eq!(decide(Some(0.5), &cfg), None);
        assert_eq!(decide(None, &cfg), None);
        assert_eq!(decide(Some(1.5), &cfg), None);
        assert_eq!(decide(Some(f64::NAN), &cfg), None);
    }

    #[test]
    fn extract_noul_accepts_shapes() {
        let v = json!({"answers": {"auto_approve": {"probability": 0.9}}});
        assert_eq!(extract_noul(&v, "auto_approve"), Some(0.9));
        let v = json!({"auto_approve": 0.7});
        assert_eq!(extract_noul(&v, "auto_approve"), Some(0.7));
        let v = json!({"answers": {"auto_approve": {"noul": {"p": 0.3}}}});
        assert_eq!(extract_noul(&v, "auto_approve"), Some(0.3));
        let v = json!({"answers": {}});
        assert_eq!(extract_noul(&v, "auto_approve"), None);
    }

    #[test]
    fn request_body_truncates_long_input() {
        let cfg = Config {
            max_context_chars: 16,
            ..test_config()
        };
        let body = build_request_body(
            &cfg,
            "bash",
            &json!({"command": "echo hello world, this is long"}),
            "bash(*)",
            "code",
            "/tmp",
        );
        let args = body
            .get("state")
            .and_then(|s| s.get("input_args"))
            .and_then(|a| a.as_str())
            .unwrap();
        assert!(args.chars().count() <= 16);
        assert_eq!(
            body.get("questions")
                .and_then(|q| q.get("auto_approve"))
                .and_then(|q| q.get("type"))
                .and_then(|t| t.as_str()),
            Some("noul")
        );
    }

    #[test]
    fn responses_serialise_to_single_line() {
        for resp in [
            json!({"ok": true, "decision": "allow"}),
            json!({"ok": true, "decision": Value::Null}),
            err("multi\nline\nerror"),
        ] {
            let s = serde_json::to_string(&resp).unwrap();
            assert!(!s.contains('\n'), "response leaks newline: {s}");
            assert!(serde_json::from_str::<Value>(&s).is_ok());
        }
    }

    #[tokio::test]
    async fn unknown_action_reports_name() {
        let cfg = test_config();
        let client = reqwest::Client::new();
        let resp = respond_to_line(&client, &cfg, r#"{"action":"frobnicate"}"#).await;
        assert_eq!(resp["ok"], false);
        assert!(
            resp["error"].as_str().unwrap().contains("frobnicate"),
            "got {resp}"
        );
    }

    #[tokio::test]
    async fn invalid_json_never_panics() {
        let cfg = test_config();
        let client = reqwest::Client::new();
        for bad in ["{not json", "", "   ", "[1,2,3]", "42", "\"str\""] {
            // Empty/whitespace lines are skipped by the main loop, but the
            // parser itself must never panic on them either.
            let resp = respond_to_line(&client, &cfg, bad).await;
            assert_eq!(resp["ok"], false, "input {bad:?} gave {resp}");
        }
    }

    #[tokio::test]
    async fn decide_without_key_returns_null() {
        let cfg = Config {
            api_key: None,
            ..test_config()
        };
        let client = reqwest::Client::new();
        let resp = respond_to_line(
            &client,
            &cfg,
            r#"{"action":"decide","tool":"bash","input":{"command":"ls"},"pattern":"bash(*)","agent":"code","directory":"/tmp"}"#,
        )
        .await;
        assert_eq!(resp["ok"], true);
        assert!(resp["decision"].is_null(), "got {resp}");
    }

    #[tokio::test]
    async fn decide_when_disabled_returns_null() {
        let cfg = Config {
            enabled: false,
            ..test_config()
        };
        let client = reqwest::Client::new();
        let resp = respond_to_line(
            &client,
            &cfg,
            r#"{"action":"decide","tool":"bash","input":{"command":"ls"},"pattern":"bash(*)","agent":"code","directory":"/tmp"}"#,
        )
        .await;
        assert_eq!(resp["ok"], true);
        assert!(resp["decision"].is_null(), "got {resp}");
    }

    #[tokio::test]
    async fn status_reports_readiness() {
        let client = reqwest::Client::new();
        let resp = respond_to_line(&client, &test_config(), r#"{"action":"status"}"#).await;
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["status"], "ready");
        let disabled = Config {
            enabled: false,
            ..test_config()
        };
        let resp = respond_to_line(&client, &disabled, r#"{"action":"status"}"#).await;
        assert_eq!(resp["status"], "disabled");
    }
}
