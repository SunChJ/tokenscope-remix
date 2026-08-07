//! Provider subscription quota, sourced from the HappyUsage `hu` CLI.
//!
//! `hu` owns credential discovery, OAuth refresh, and provider API calls.
//! Tokenscope invokes `hu usage <provider> --json`, parses the envelope, and
//! keeps the result in memory. There is deliberately no local fallback: when
//! `hu` cannot serve a provider, that provider shows as unavailable.

use crate::model::{same_reset_cycle, LimitWindow, ProviderLimit, QuotaTrendPoint, WEEKLY_WINDOW_MINUTES};
use serde_json::Value;
use std::fs;
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

fn hu_executables() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    // 1. Bundled copy (app Resources/bin/hu) — the primary source so the app
    //    is self-contained and the hu version ships with Tokenscope.
    if let Some(path) = bundled_hu() {
        candidates.push(path);
    }
    // 2. System installs (PATH + common locations; install.sh drops `hu` into
    //    ~/.local/bin, which may not be on the app's PATH at launch).
    if let Some(path) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(if cfg!(windows) { "hu.exe" } else { "hu" }))
            .find(|path| path.is_file())
    }) {
        if !candidates.contains(&path) {
            candidates.push(path);
        }
    }
    let mut extras = vec![
        PathBuf::from("/opt/homebrew/bin/hu"),
        PathBuf::from("/usr/local/bin/hu"),
    ];
    if let Some(home) = dirs::home_dir() {
        extras.push(home.join(".local").join("bin").join("hu"));
        extras.push(home.join("bin").join("hu"));
        extras.push(home.join("go").join("bin").join("hu"));
    }
    for path in extras {
        if path.is_file() && !candidates.contains(&path) {
            candidates.push(path);
        }
    }
    candidates
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

const HAPPYUSAGE_INSTALL_URL: &str =
    "https://raw.githubusercontent.com/SunChJ/happyusage/main/scripts/install.sh";
const INSTALL_RETRY_MS: i64 = 24 * 60 * 60 * 1000;
static LAST_INSTALL_ATTEMPT_MS: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(0);

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Make sure `hu` exists, installing it once per 24h when missing. Returns
/// true when a usable executable is present afterwards. Runs on the
/// background refresh thread; a slow brew install only delays this refresh.
fn ensure_hu() -> bool {
    if !hu_executables().is_empty() {
        return true;
    }
    let now = now_ms();
    let last = LAST_INSTALL_ATTEMPT_MS.load(std::sync::atomic::Ordering::Relaxed);
    if now - last < INSTALL_RETRY_MS {
        return false;
    }
    let installed = try_install_hu();
    LAST_INSTALL_ATTEMPT_MS.store(now, std::sync::atomic::Ordering::Relaxed);
    installed
}

fn try_install_hu() -> bool {
    // 1. Homebrew: tap + install (covers macOS and Linux with brew).
    if let Some(brew) = find_on_path("brew") {
        let tap_ok = Command::new(&brew)
            .args(["tap", "SunChJ/happyusage"])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if tap_ok
            && Command::new(&brew)
                .args(["install", "hu"])
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
            && !hu_executables().is_empty()
        {
            return true;
        }
    }
    // 2. Official install script (needs curl + sh): fetch, run, then re-scan.
    if let Some(curl) = find_on_path("curl") {
        if let Ok(script) = Command::new(&curl).args(["-fsSL", HAPPYUSAGE_INSTALL_URL]).output() {
            if script.status.success() {
                let tmp = std::env::temp_dir().join(format!(
                    "happyusage-install-{}.sh",
                    std::process::id()
                ));
                if fs::write(&tmp, &script.stdout).is_ok() {
                    let ok = Command::new("sh")
                        .arg(&tmp)
                        .status()
                        .map(|status| status.success())
                        .unwrap_or(false);
                    let _ = fs::remove_file(tmp);
                    if ok && !hu_executables().is_empty() {
                        return true;
                    }
                }
            }
        }
    }
    // 3. Go toolchain as a last resort.
    if let Some(go) = find_on_path("go") {
        if Command::new(&go)
            .args(["install", "github.com/SunChJ/happyusage/cmd/hu@latest"])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
            && !hu_executables().is_empty()
        {
            return true;
        }
    }
    false
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }))
        .find(|path| path.is_file())
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

/// Refresh provider limits, installing `hu` first when missing. Each provider
/// is independent: `hu` is the only data source, so a failed fetch leaves that
/// provider unavailable (no local log or cache fallback).
pub fn reload() -> bool {
    let previous = cache_lock()
        .read()
        .map(|cache| cache.clone())
        .unwrap_or_default();
    let mut next = previous.clone();

    // First reload attempt: make sure `hu` exists (auto-install when missing).
    ensure_hu();

    if let Some(claude) = run_hu("claude").and_then(|envelope| claude_from_hu(&envelope)) {
        next.claude = Some(claude);
    } else {
        // `hu` does not serve Claude usage right now (expired OAuth, rate
        // limit, missing subscription): leave it unavailable instead of
        // surfacing stale local data.
        next.claude = None;
    }
    if let Some(codex) = run_hu("codex").and_then(|envelope| codex_from_hu(&envelope)) {
        next.codex = Some(codex);
    } else {
        next.codex = None;
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
    use super::{claude_from_hu, codex_from_hu};
    use crate::model::WEEKLY_WINDOW_MINUTES;

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
