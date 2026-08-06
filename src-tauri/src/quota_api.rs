//! Provider subscription quota, sourced from the HappyUsage `hu` CLI.
//!
//! `hu` already owns credential discovery, OAuth refresh, and provider API
//! calls (Claude via the Anthropic OAuth usage API, Codex via the ChatGPT
//! usage API). Tokenscope only invokes `hu usage <provider> --json`, parses
//! the envelope, and caches the result so a later offline refresh still has
//! the last successful reading. Codex session-log rate limits remain a
//! fallback in the parser.

use crate::model::{same_reset_cycle, LimitWindow, ProviderLimit, QuotaTrendPoint, WEEKLY_WINDOW_MINUTES};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{OnceLock, RwLock};

const CACHE_FILE: &str = "provider_limits.json";
const CACHE_VERSION: u32 = 2;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ProviderCache {
    claude: Option<ProviderLimit>,
    codex: Option<ProviderLimit>,
}

static CACHE: OnceLock<RwLock<ProviderCache>> = OnceLock::new();

fn cache_dir() -> Option<PathBuf> {
    let dir = dirs::cache_dir()?.join("tokenscope");
    let _ = fs::create_dir_all(&dir);
    Some(dir)
}

fn cache_path() -> Option<PathBuf> {
    Some(cache_dir()?.join(CACHE_FILE))
}

fn load_cache() -> ProviderCache {
    let Some(path) = cache_path() else {
        return ProviderCache::default();
    };
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<CacheEnvelope>(&text).ok())
        .filter(|cache| cache.version == CACHE_VERSION)
        .map(|cache| cache.cache)
        .unwrap_or_default()
}

#[derive(Serialize, Deserialize)]
struct CacheEnvelope {
    version: u32,
    cache: ProviderCache,
}

fn cache_lock() -> &'static RwLock<ProviderCache> {
    CACHE.get_or_init(|| RwLock::new(load_cache()))
}

/// Last successfully fetched provider limits (disk + memory). Empty when no
/// provider has ever produced a usable snapshot.
pub fn shared() -> Vec<ProviderLimit> {
    let cache = cache_lock()
        .read()
        .map(|cache| cache.clone())
        .unwrap_or_default();
    let mut limits = Vec::new();
    if let Some(claude) = cache.claude {
        limits.push(claude);
    }
    if let Some(codex) = cache.codex {
        limits.push(codex);
    }
    limits
}

fn save_cache(cache: &ProviderCache) {
    let Some(path) = cache_path() else { return };
    let envelope = CacheEnvelope {
        version: CACHE_VERSION,
        cache: cache.clone(),
    };
    let Ok(text) = serde_json::to_vec(&envelope) else {
        return;
    };
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    if fs::write(&tmp, text).is_ok() && fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(tmp);
    }
}

fn hu_executables() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(if cfg!(windows) { "hu.exe" } else { "hu" }))
            .find(|path| path.is_file())
    }) {
        candidates.push(path);
    }
    for path in ["/opt/homebrew/bin/hu", "/usr/local/bin/hu"] {
        let path = PathBuf::from(path);
        if path.is_file() && !candidates.contains(&path) {
            candidates.push(path);
        }
    }
    candidates
}

/// Run `hu usage <provider> --json` and return the provider envelope
/// (with `ok: true`). The CLI owns credential handling and HTTP timeouts;
/// Tokenscope's refresh runs on a background thread, so a slow run only
/// delays that provider's next refresh.
fn run_hu(provider: &str) -> Option<Value> {
    for executable in hu_executables() {
        let output = Command::new(executable)
            .args(["usage", provider, "--json"])
            .output()
            .ok()?;
        if !output.status.success() {
            continue;
        }
        let json: Value = serde_json::from_slice(&output.stdout).ok()?;
        let envelope = json.get("provider")?;
        if envelope.get("ok").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        return Some(envelope.clone());
    }
    None
}

fn rfc3339_ms(value: Option<&Value>) -> i64 {
    value
        .and_then(Value::as_str)
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|date| date.timestamp_millis())
        .unwrap_or(0)
}

/// Claude windows from the `hu` envelope. `session` → 5-hour, `weekly` → 7d.
/// Matching is case-insensitive and ignores model-scoped sub-pools
/// (sonnet_weekly / opus_weekly / …) so a renamed quota never breaks the card.
fn claude_from_hu(envelope: &Value) -> Option<ProviderLimit> {
    let checked_at = rfc3339_ms(envelope.get("checked_at"));
    let plan = envelope
        .get("plan")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut windows = Vec::new();
    for quota in envelope.get("quotas")?.as_array()? {
        let name = quota.get("name").and_then(Value::as_str)?;
        let lower = name.to_ascii_lowercase();
        let (id, label, duration) = if lower.contains("session") {
            ("5h", "5-hour", 300)
        } else if lower == "weekly" {
            ("weekly", "Weekly", WEEKLY_WINDOW_MINUTES)
        } else {
            // Model-scoped weekly sub-pools (sonnet/opus/…) are not shown.
            continue;
        };
        let used_pct = quota.get("used_pct").and_then(Value::as_f64)?;
        let resets_at = rfc3339_ms(quota.get("resets_at")) / 1000;
        windows.push(LimitWindow {
            id: id.to_string(),
            label: label.to_string(),
            duration_minutes: duration,
            used_pct,
            resets_at,
            as_of_ms: checked_at,
            trend: Vec::new(),
        });
    }
    if windows.is_empty() {
        return None;
    }
    Some(ProviderLimit {
        provider: "claude".to_string(),
        label: "Claude".to_string(),
        plan,
        windows,
    })
}

/// Claude windows from Claude Code's local `~/.claude.json` usage cache.
/// Used only when `hu` cannot reach the API (e.g. expired OAuth token).
fn claude_from_cache_file() -> Option<ProviderLimit> {
    let home = dirs::home_dir()?;
    let json: Value = serde_json::from_str(
        &fs::read_to_string(home.join(".claude.json")).ok()?,
    )
    .ok()?;
    let cache = json.get("cachedUsageUtilization")?;
    let fetched_at = cache.get("fetchedAtMs").and_then(Value::as_i64)?;
    let utilization = cache.get("utilization")?;
    let mut windows = Vec::new();
    for (key, id, label, duration) in [
        ("five_hour", "5h", "5-hour", 300),
        (
            "seven_day",
            "weekly",
            "Weekly",
            WEEKLY_WINDOW_MINUTES as i64,
        ),
    ] {
        let Some(window) = utilization.get(key) else { continue };
        let Some(used_pct) = window.get("utilization").and_then(Value::as_f64) else {
            continue;
        };
        let resets_at = rfc3339_ms(window.get("resets_at")) / 1000;
        if resets_at <= 0 {
            continue;
        }
        windows.push(LimitWindow {
            id: id.to_string(),
            label: label.to_string(),
            duration_minutes: duration as u64,
            used_pct,
            resets_at,
            as_of_ms: fetched_at,
            trend: Vec::new(),
        });
    }
    if windows.is_empty() {
        return None;
    }
    Some(ProviderLimit {
        provider: "claude".to_string(),
        label: "Claude".to_string(),
        plan: String::new(),
        windows,
    })
}

/// Codex windows from the `hu` envelope. The primary pool and Spark are both
/// rolling weekly windows today (the 5h window is retired); HappyUsage's quota
/// names have changed across versions (`session`/`weekly`, `Spark`/`Spark_weekly`),
/// so matching is case-insensitive and name-agnostic.
fn codex_from_hu(envelope: &Value) -> Option<ProviderLimit> {
    let checked_at = rfc3339_ms(envelope.get("checked_at"));
    let plan = envelope
        .get("plan")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut windows = Vec::new();
    for quota in envelope.get("quotas")?.as_array()? {
        let name = quota.get("name").and_then(Value::as_str)?;
        let lower = name.to_ascii_lowercase();
        let (id, label) = if lower.contains("spark") {
            ("spark", "Spark")
        } else if lower == "session" || lower == "weekly" {
            ("weekly", "Weekly")
        } else {
            continue;
        };
        let used_pct = quota.get("used_pct").and_then(Value::as_f64)?;
        let resets_at = rfc3339_ms(quota.get("resets_at")) / 1000;
        windows.push(LimitWindow {
            id: id.to_string(),
            label: label.to_string(),
            duration_minutes: WEEKLY_WINDOW_MINUTES,
            used_pct,
            resets_at,
            as_of_ms: checked_at,
            trend: Vec::new(),
        });
    }
    if windows.is_empty() {
        return None;
    }
    Some(ProviderLimit {
        provider: "codex".to_string(),
        label: "Codex".to_string(),
        plan,
        windows,
    })
}

/// Merge the Codex weekly trend points from the process cache into the window
/// trend. Claude has no history source, so its windows stay trend-free.
fn merge_trend(
    previous: &ProviderCache,
    next: &mut ProviderCache,
) {
    let Some(codex) = next.codex.as_mut() else { return };
    let previous_resets: Vec<(String, i64)> = previous
        .codex
        .as_ref()
        .map(|limit| {
            limit
                .windows
                .iter()
                .map(|window| (window.id.clone(), window.resets_at))
                .collect()
        })
        .unwrap_or_default();
    for window in &mut codex.windows {
        if window.resets_at <= 0 {
            continue;
        }
        let same_cycle = previous_resets
            .iter()
            .find(|(id, _)| id == &window.id)
            .map(|(_, reset)| {
                same_reset_cycle(*reset, window.resets_at)
            })
            .unwrap_or(false);
        if !same_cycle {
            window.trend.clear();
        }
        window.trend.push(QuotaTrendPoint {
            ts_ms: window.as_of_ms,
            used_pct: window.used_pct,
        });
        window.trend.sort_by_key(|point| point.ts_ms);
        window.trend.dedup_by(|right, left| {
            if right.ts_ms == left.ts_ms {
                left.used_pct = right.used_pct;
                true
            } else {
                false
            }
        });
        const MAX_POINTS: usize = 48;
        if window.trend.len() > MAX_POINTS {
            let source = window.trend.len() - MAX_POINTS;
            window.trend = window.trend[source..].to_vec();
        }
    }
}

/// Refresh provider limits through `hu`. Each provider is independent: a
/// failure keeps the previous successful snapshot for that provider.
pub fn reload() -> bool {
    let previous = cache_lock()
        .read()
        .map(|cache| cache.clone())
        .unwrap_or_default();
    let mut next = previous.clone();

    if let Some(claude) = run_hu("claude").and_then(|envelope| claude_from_hu(&envelope)) {
        next.claude = Some(claude);
    } else if next.claude.is_none() {
        // `hu` cannot refresh Claude (expired OAuth) — fall back to the local
        // Claude Code usage cache so the card still shows the last reading.
        next.claude = claude_from_cache_file();
    }
    if let Some(codex) = run_hu("codex").and_then(|envelope| codex_from_hu(&envelope)) {
        next.codex = Some(codex);
    }

    let changed = next.claude.is_some() || next.codex.is_some();
    if !changed {
        return false;
    }
    merge_trend(&previous, &mut next);
    save_cache(&next);
    if let Ok(mut cache) = cache_lock().write() {
        *cache = next;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_envelope_windows() {
        let envelope = serde_json::json!({
            "provider": "claude",
            "ok": true,
            "checked_at": "2026-08-06T09:16:00Z",
            "plan": "Pro",
            "quotas": [
                {"name": "session", "period": "5h", "used_pct": 12.5, "left_pct": 87.5, "resets_at": "2026-08-06T14:16:00Z"},
                {"name": "weekly", "period": "7d", "used_pct": 43, "left_pct": 57, "resets_at": "2026-08-09T21:00:00Z"}
            ]
        });
        let limit = claude_from_hu(&envelope).unwrap();
        assert_eq!(limit.provider, "claude");
        assert_eq!(limit.windows.len(), 2);
        assert_eq!(limit.windows[0].id, "5h");
        assert_eq!(limit.windows[0].duration_minutes, 300);
        assert_eq!(limit.windows[0].used_pct, 12.5);
        assert_eq!(limit.windows[1].id, "weekly");
        assert_eq!(limit.windows[1].duration_minutes, WEEKLY_WINDOW_MINUTES);
        assert_eq!(limit.windows[1].resets_at, 1_786_309_200); // 2026-08-09T21:00:00Z
    }

    #[test]
    fn parses_codex_envelope_as_weekly_windows() {
        let envelope = serde_json::json!({
            "provider": "codex",
            "ok": true,
            "checked_at": "2026-08-06T09:16:00Z",
            "plan": "Pro 5x",
            "quotas": [
                {"name": "session", "period": "5h", "used_pct": 71, "left_pct": 29, "resets_at": "2026-08-09T12:01:05Z"},
                {"name": "Spark", "period": "5h", "used_pct": 69, "left_pct": 31, "resets_at": "2026-08-08T08:01:04Z"}
            ]
        });
        let limit = codex_from_hu(&envelope).unwrap();
        assert_eq!(limit.provider, "codex");
        assert_eq!(limit.windows.len(), 2);
        assert_eq!(limit.windows[0].id, "weekly");
        assert_eq!(limit.windows[0].duration_minutes, WEEKLY_WINDOW_MINUTES);
        assert_eq!(limit.windows[0].used_pct, 71.0);
        assert_eq!(limit.windows[1].id, "spark");
        assert_eq!(limit.windows[1].used_pct, 69.0);
    }

    #[test]
    fn parses_claude_local_cache_shape() {
        let json = serde_json::json!({
            "cachedUsageUtilization": {
                "fetchedAtMs": 1_785_485_070_985i64,
                "utilization": {
                    "five_hour": {"utilization": 0, "resets_at": "2026-07-31T12:50:00Z"},
                    "seven_day": {"utilization": 21, "resets_at": "2026-07-31T21:00:00Z"}
                }
            }
        });
        let cache = json.get("cachedUsageUtilization").unwrap();
        let fetched_at = cache.get("fetchedAtMs").and_then(Value::as_i64).unwrap();
        let utilization = cache.get("utilization").unwrap();
        let mut windows = Vec::new();
        for (key, id, label, duration) in [
            ("five_hour", "5h", "5-hour", 300),
            ("seven_day", "weekly", "Weekly", WEEKLY_WINDOW_MINUTES as i64),
        ] {
            let Some(window) = utilization.get(key) else { continue };
            let Some(used_pct) = window.get("utilization").and_then(Value::as_f64) else {
                continue;
            };
            let resets_at = rfc3339_ms(window.get("resets_at")) / 1000;
            windows.push(LimitWindow {
                id: id.to_string(),
                label: label.to_string(),
                duration_minutes: duration as u64,
                used_pct,
                resets_at,
                as_of_ms: fetched_at,
                trend: Vec::new(),
            });
        }
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].id, "5h");
        assert_eq!(windows[0].used_pct, 0.0);
        assert_eq!(windows[1].id, "weekly");
        assert_eq!(windows[1].used_pct, 21.0);
        assert_eq!(windows[1].duration_minutes, WEEKLY_WINDOW_MINUTES);
    }

    #[test]
    fn parses_codex_0211_names_and_ignores_missing_provider() {
        // happyusage 0.2.11 renames primary to `weekly` and Spark to
        // `Spark_weekly`; both must still map to the same windows.
        let envelope = serde_json::json!({
            "provider": "codex",
            "ok": true,
            "checked_at": "2026-08-06T09:42:16Z",
            "plan": "Pro 5x",
            "quotas": [
                {"name": "weekly", "period": "7d", "used_pct": 71, "left_pct": 29, "resets_at": "2026-08-09T12:01:06Z"},
                {"name": "Spark_weekly", "period": "7d", "used_pct": 69, "left_pct": 31, "resets_at": "2026-08-08T08:01:04Z"}
            ]
        });
        let limit = codex_from_hu(&envelope).unwrap();
        assert_eq!(limit.windows.len(), 2);
        assert_eq!(limit.windows[0].id, "weekly");
        assert_eq!(limit.windows[1].id, "spark");
        assert_eq!(limit.windows[1].used_pct, 69.0);

        // A provider that only has unknown quota names yields no windows.
        let empty = serde_json::json!({"provider": "codex", "ok": true, "quotas": []});
        assert!(codex_from_hu(&empty).is_none());
        let unknown = serde_json::json!({
            "provider": "codex",
            "ok": true,
            "quotas": [{"name": "gpt_6_daily", "used_pct": 5}]
        });
        assert!(codex_from_hu(&unknown).is_none());
    }

    #[test]
    fn claude_ignores_model_scoped_sub_pools() {
        let envelope = serde_json::json!({
            "provider": "claude",
            "ok": true,
            "checked_at": "2026-08-06T09:16:00Z",
            "quotas": [
                {"name": "session", "period": "5h", "used_pct": 12.5, "resets_at": "2026-08-06T14:16:00Z"},
                {"name": "weekly", "period": "7d", "used_pct": 43, "resets_at": "2026-08-09T21:00:00Z"},
                {"name": "sonnet_weekly", "period": "7d", "used_pct": 99, "resets_at": "2026-08-09T21:00:00Z"}
            ]
        });
        let limit = claude_from_hu(&envelope).unwrap();
        assert_eq!(limit.windows.len(), 2);
        assert!(limit.windows.iter().all(|w| w.id != "sonnet_weekly"));
    }

    #[test]
    fn ignores_unknown_quota_names() {
        let envelope = serde_json::json!({
            "provider": "claude",
            "ok": true,
            "checked_at": "2026-08-06T09:16:00Z",
            "quotas": [{"name": "some_new_pool", "used_pct": 5}]
        });
        assert!(claude_from_hu(&envelope).is_none());
    }
}
