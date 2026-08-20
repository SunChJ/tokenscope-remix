//! Provider subscription quota, sourced from the HappyUsage `hu` CLI.
//!
//! `hu` owns credential discovery, OAuth refresh, and provider API calls.
//! Tokenscope invokes `hu usage <provider> --json`, parses the envelope, and
//! keeps the result in memory. There is deliberately no local fallback: when
//! `hu` cannot serve a provider, that provider shows as unavailable.

use crate::model::{same_reset_cycle, LimitWindow, ProviderLimit, QuotaTrendPoint, WEEKLY_WINDOW_MINUTES};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{OnceLock, RwLock};

#[derive(Clone, Default)]
struct ProviderCache {
    claude: Option<ProviderLimit>,
    codex: Option<ProviderLimit>,
}

static CACHE: OnceLock<RwLock<ProviderCache>> = OnceLock::new();

fn cache_lock() -> &'static RwLock<ProviderCache> {
    CACHE.get_or_init(|| RwLock::new(ProviderCache::default()))
}

/// Last successfully fetched provider limits (in-memory only). Empty when no
/// provider has ever produced a usable snapshot in this process.
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

/// Directory injected by lib.rs at startup: `app.path().resource_dir()`.
static BUNDLE_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

pub fn set_bundle_dir(dir: Option<PathBuf>) {
    let _ = BUNDLE_DIR.set(dir);
}

fn bundled_hu() -> Option<PathBuf> {
    let dir = BUNDLE_DIR.get()?;
    let base = dir.as_ref()?;
    let path = base
        .join("bin")
        .join(if cfg!(windows) { "hu.exe" } else { "hu" });
    path.is_file().then_some(path)
}

/// Run `hu usage codex claude --json` once and return the full envelope. One
/// invocation covers both displayed providers in a single process (OAuth
/// discovery + provider API calls shared) instead of one spawn per provider.
/// The CLI owns credential handling and HTTP timeouts; Tokenscope's refresh
/// runs on a background thread, so a slow run only delays the next refresh.
fn run_hu() -> Option<Value> {
    // Tokenscope owns its HappyUsage version. Never fall back to a binary from
    // PATH/Homebrew because its version and output schema are not controlled.
    let executable = bundled_hu()?;
    let output = Command::new(executable)
        .args(["usage", "codex", "claude", "--json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// Per-provider envelopes from a `hu usage --json` response. Current `hu`
/// emits a `providers` array (one entry per configured provider); tolerate a
/// singular `provider` object for older builds.
fn provider_envelopes(json: &Value) -> Vec<Value> {
    if let Some(array) = json.get("providers").and_then(Value::as_array) {
        return array.clone();
    }
    json.get("provider").cloned().into_iter().collect()
}

/// Map one provider envelope to a parsed limit. Unknown providers (cursor,
/// copilot, …) and failed fetches (`ok: false`) yield None, leaving that
/// provider unavailable instead of surfacing stale data.
fn parse_provider(envelope: &Value) -> Option<ProviderLimit> {
    if envelope.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    match envelope.get("provider").and_then(Value::as_str) {
        Some("claude") => claude_from_hu(envelope),
        Some("codex") => codex_from_hu(envelope),
        _ => None,
    }
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

/// Codex's account-wide primary pool is weekly today. Model-scoped pools such
/// as Spark are intentionally ignored: they do not represent general Codex
/// availability and must not override the menu-bar provider summary.
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
        if lower != "session" && lower != "weekly" {
            continue;
        }
        let used_pct = quota.get("used_pct").and_then(Value::as_f64)?;
        let resets_at = rfc3339_ms(quota.get("resets_at")) / 1000;
        windows.push(LimitWindow {
            id: "weekly".to_string(),
            label: "Weekly".to_string(),
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

/// Merge the Codex weekly trend points from the process cache into the window.
/// Claude has no history source, so its windows stay trend-free.
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

/// Split one `hu usage` response into parsed Claude/Codex limits. A provider
/// yields None when absent from the array (`hu` drops failed providers), when
/// its envelope reports `ok: false`, or when no usable windows parsed.
fn parse_response(json: &Value) -> (Option<ProviderLimit>, Option<ProviderLimit>) {
    let mut claude = None;
    let mut codex = None;
    for envelope in provider_envelopes(json) {
        match envelope.get("provider").and_then(Value::as_str) {
            Some("claude") => claude = parse_provider(&envelope),
            Some("codex") => codex = parse_provider(&envelope),
            // Cursor/copilot/… are fetched by `hu` but not shown yet.
            _ => {}
        }
    }
    (claude, codex)
}

/// Refresh provider limits, installing `hu` first when missing. A single
/// `hu usage codex claude --json` run covers both displayed providers in one
/// process; each provider is independent, so a provider absent from the
/// response or reported `ok: false` becomes unavailable without affecting the
/// other. `hu` drops failed providers from the array entirely, so a transient
/// miss (rate limit, expired OAuth) shows as unavailable until it recovers.
pub fn reload() -> bool {
    let previous = cache_lock()
        .read()
        .map(|cache| cache.clone())
        .unwrap_or_default();
    let mut next = previous.clone();

    if let Some(json) = run_hu() {
        let (claude, codex) = parse_response(&json);
        next.claude = claude;
        next.codex = codex;
    }

    let changed = next.claude.is_some() || next.codex.is_some();
    if !changed {
        return false;
    }
    merge_trend(&previous, &mut next);
    if let Ok(mut cache) = cache_lock().write() {
        *cache = next;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{claude_from_hu, codex_from_hu, parse_provider, parse_response, provider_envelopes};
    use crate::model::WEEKLY_WINDOW_MINUTES;

    #[test]
    fn parses_multi_provider_envelope_and_ignores_unknown() {
        // `hu usage codex claude --json` returns the requested providers in one array.
        let envelope = serde_json::json!({
            "ok": true,
            "source": "native_provider_scripts",
            "checked_at": "2026-08-11T08:47:00Z",
            "provider_count": 2,
            "providers": [
                {"provider": "codex", "ok": true, "checked_at": "2026-08-11T08:47:11Z", "plan": "Pro 5x",
                 "quotas": [
                    {"name": "weekly", "period": "7d", "used_pct": 34, "left_pct": 66, "resets_at": "2026-08-18T01:39:45Z"},
                    {"name": "Spark_weekly", "period": "7d", "used_pct": 0, "left_pct": 100, "resets_at": "2026-08-18T08:47:12Z"}
                 ]},
                {"provider": "cursor", "ok": true, "checked_at": "2026-08-11T08:47:05Z", "plan": "Free",
                 "quotas": [{"name": "total", "period": "monthly", "used_pct": 0, "left_pct": 100, "resets_at": "2026-08-15T11:57:01Z"}]}
            ]
        });
        let (claude, codex) = parse_response(&envelope);
        assert!(claude.is_none());
        let codex = codex.unwrap();
        assert_eq!(codex.provider, "codex");
        assert_eq!(codex.windows.len(), 1);
        assert_eq!(codex.windows[0].id, "weekly");
    }

    #[test]
    fn failed_provider_stays_unavailable_without_taking_others_down() {
        let envelope = serde_json::json!({
            "ok": true,
            "providers": [
                {"provider": "claude", "ok": false, "error": "expired OAuth"},
                {"provider": "codex", "ok": true, "checked_at": "2026-08-11T08:47:11Z", "plan": "Pro 5x",
                 "quotas": [{"name": "weekly", "period": "7d", "used_pct": 34, "resets_at": "2026-08-18T01:39:45Z"}]}
            ]
        });
        let (claude, codex) = parse_response(&envelope);
        // The failed Claude stays None, so reload() marks it unavailable;
        // the working Codex still comes through.
        assert!(claude.is_none());
        assert!(codex.is_some());
    }

    #[test]
    fn tolerates_singular_provider_envelope() {
        let envelope = serde_json::json!({
            "provider": {"provider": "claude", "ok": true, "checked_at": "2026-08-06T09:16:00Z", "plan": "Pro",
             "quotas": [{"name": "session", "period": "5h", "used_pct": 12.5, "resets_at": "2026-08-06T14:16:00Z"}]}
        });
        let providers = provider_envelopes(&envelope);
        assert_eq!(providers.len(), 1);
        assert_eq!(parse_provider(&providers[0]).unwrap().provider, "claude");
    }

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
    fn parses_legacy_codex_session_and_ignores_spark() {
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
        assert_eq!(limit.windows.len(), 1);
        assert_eq!(limit.windows[0].id, "weekly");
        assert_eq!(limit.windows[0].duration_minutes, WEEKLY_WINDOW_MINUTES);
        assert_eq!(limit.windows[0].used_pct, 71.0);
    }

    #[test]
    fn ignores_spark_quota_windows() {
        let envelope = serde_json::json!({
            "provider": "codex",
            "ok": true,
            "checked_at": "2026-08-20T01:57:21Z",
            "plan": "Pro 5x",
            "quotas": [
                {"name": "weekly", "period": "7d", "used_pct": 96, "left_pct": 4, "resets_at": "2026-08-20T03:44:48Z"},
                {"name": "Spark", "period": "5h", "used_pct": 0, "left_pct": 100, "resets_at": "2026-08-20T06:57:22Z"},
                {"name": "Spark_weekly", "period": "7d", "used_pct": 100, "left_pct": 0, "resets_at": "2026-08-20T06:23:42Z"}
            ]
        });
        let limit = codex_from_hu(&envelope).unwrap();
        assert_eq!(limit.windows.len(), 1);
        assert_eq!(limit.windows[0].id, "weekly");
        assert_eq!(limit.windows[0].used_pct, 96.0);
    }

    #[test]
    fn parses_codex_weekly_and_ignores_missing_provider() {
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
        assert_eq!(limit.windows.len(), 1);
        assert_eq!(limit.windows[0].id, "weekly");
        assert_eq!(limit.windows[0].used_pct, 71.0);

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
