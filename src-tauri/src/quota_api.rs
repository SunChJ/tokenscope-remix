//! Provider subscription quota, sourced from the HappyUsage `hu` CLI.
//!
//! `hu` owns credential discovery, OAuth refresh, and provider API calls.
//! Tokenscope invokes `hu usage <provider> --json`, parses the envelope, and
//! caches the result. When `hu` is missing or fails, Codex falls back to the
//! native wham/usage call (Pi / Codex CLI OAuth) so users without HappyUsage
//! still get quota; Claude falls back to Claude Code's local usage cache.
//! Codex session-log rate limits remain a parser-level fallback.

use crate::model::{same_reset_cycle, LimitWindow, ProviderLimit, QuotaTrendPoint, WEEKLY_WINDOW_MINUTES};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
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

/// Refresh provider limits, installing `hu` first when missing. Each provider
/// is independent: a failure keeps the previous successful snapshot. When `hu`
/// is absent or its Codex call fails, the native wham/usage call is the next
/// fallback, and Claude falls back to Claude Code's local usage cache.
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
    } else if next.claude.is_none() {
        // `hu` cannot refresh Claude (expired OAuth) — fall back to the local
        // Claude Code usage cache so the card still shows the last reading.
        next.claude = claude_from_cache_file();
    }
    if let Some(codex) = run_hu("codex").and_then(|envelope| codex_from_hu(&envelope)) {
        next.codex = Some(codex);
    } else if next.codex.is_none() {
        // No `hu` (or its Codex call failed): try the native usage API using
        // the Pi / Codex CLI OAuth credentials.
        next.codex = codex_from_native();
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

// ── Native Codex fallback (no HappyUsage) ────────────────────────────

#[derive(Clone)]
struct Credential {
    access: String,
    account_id: String,
    refresh: String,
}

fn pi_agent_dir() -> Option<PathBuf> {
    std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .or_else(|| Some(dirs::home_dir()?.join(".pi").join("agent")))
}

fn jwt_account_id(token: &str) -> String {
    let Some(payload) = token.split('.').nth(1) else {
        return String::new();
    };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else {
        return String::new();
    };
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|json| {
            json.get("https://api.openai.com/auth")?
                .get("chatgpt_account_id")?
                .as_str()
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

/// Pi's own `openai-codex` OAuth credential (~/.pi/agent/auth.json).
fn read_pi_credential() -> Option<Credential> {
    let path = pi_agent_dir()?.join("auth.json");
    let json: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    let auth = json.get("openai-codex")?;
    if auth.get("type").and_then(Value::as_str) != Some("oauth") {
        return None;
    }
    let access = auth.get("access")?.as_str()?.to_string();
    if access.is_empty() {
        return None;
    }
    let account_id = auth
        .get("accountId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| jwt_account_id(&access));
    Some(Credential {
        access,
        account_id,
        refresh: auth
            .get("refresh")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let text = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    if text.is_empty() || !text.is_ascii() || !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).ok())
        .collect()
}

fn parse_json_or_hex(text: &str) -> Option<Value> {
    let text = text.trim();
    serde_json::from_str(text).ok().or_else(|| {
        let bytes = decode_hex(text)?;
        serde_json::from_slice(&bytes).ok()
    })
}

fn codex_auth_paths() -> Vec<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return vec![PathBuf::from(home).join("auth.json")];
    }
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    vec![
        home.join(".config").join("codex").join("auth.json"),
        home.join(".codex").join("auth.json"),
    ]
}

fn read_codex_credential() -> Option<Credential> {
    codex_auth_paths()
        .into_iter()
        .filter_map(|path| {
            let json = parse_json_or_hex(&fs::read_to_string(&path).ok()?)?;
            let tokens = json.get("tokens")?;
            let access = tokens.get("access_token")?.as_str()?.to_string();
            if access.is_empty() {
                return None;
            }
            let account_id = tokens
                .get("account_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| jwt_account_id(&access));
            Some(Credential {
                access,
                account_id,
                refresh: tokens
                    .get("refresh_token")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .next()
}

fn pi_executables() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(if cfg!(windows) { "pi.exe" } else { "pi" }))
            .find(|path| path.is_file())
    }) {
        candidates.push(path);
    }
    for path in ["/opt/homebrew/bin/pi", "/usr/local/bin/pi"] {
        let path = PathBuf::from(path);
        if path.is_file() && !candidates.contains(&path) {
            candidates.push(path);
        }
    }
    candidates
}

/// Ask Pi itself for a fresh token so its credential lock and persistence
/// stay authoritative; Tokenscope never rewrites Pi's auth.json.
fn refreshed_pi_credential(_previous: &Credential) -> Option<Credential> {
    for executable in pi_executables() {
        let output = Command::new(executable)
            .args([
                "auth",
                "print-bearer-token",
                "--provider",
                "openai-codex",
                "--model",
                "gpt-5.5",
                "--min-expiry",
                "30m",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            continue;
        }
        let Ok(stdout) = String::from_utf8(output.stdout) else {
            continue;
        };
        let access = stdout.trim().to_string();
        if access.is_empty() {
            continue;
        }
        let stored = read_pi_credential();
        let account_id = stored
            .as_ref()
            .map(|credential| credential.account_id.clone())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| jwt_account_id(&access));
        return Some(Credential {
            access,
            account_id,
            refresh: stored
                .map(|credential| credential.refresh)
                .unwrap_or_else(|| _previous.refresh.clone()),
        });
    }
    None
}

fn persist_codex_credential(
    path: &PathBuf,
    previous_refresh: &str,
    access: &str,
    refresh: &str,
) -> bool {
    let Some(mut json) = fs::read_to_string(path)
        .ok()
        .and_then(|text| parse_json_or_hex(&text))
    else {
        return false;
    };
    let Some(tokens) = json.get_mut("tokens").and_then(Value::as_object_mut) else {
        return false;
    };
    if tokens
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        != previous_refresh
    {
        return false;
    }
    tokens.insert("access_token".to_string(), Value::String(access.to_string()));
    tokens.insert("refresh_token".to_string(), Value::String(refresh.to_string()));
    let account_id = jwt_account_id(access);
    if !account_id.is_empty() {
        tokens.insert("account_id".to_string(), Value::String(account_id));
    }
    let Ok(bytes) = serde_json::to_vec_pretty(&json) else {
        return false;
    };
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    if fs::write(&tmp, bytes).is_err() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    if fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(tmp);
        return false;
    }
    true
}

fn refreshed_codex_credential(previous: &Credential) -> Option<Credential> {
    if previous.refresh.is_empty() {
        return None;
    }
    let response = ureq::post(CODEX_TOKEN_URL)
        .timeout(Duration::from_secs(15))
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CODEX_OAUTH_CLIENT_ID),
            ("refresh_token", previous.refresh.as_str()),
        ])
        .ok()?;
    let json: Value = response.into_json().ok()?;
    let access = json.get("access_token")?.as_str()?.to_string();
    let refresh = json
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or(&previous.refresh)
        .to_string();
    let path = codex_auth_paths().into_iter().find(|path| path.is_file())?;
    if !persist_codex_credential(&path, &previous.refresh, &access, &refresh) {
        return None;
    }
    Some(Credential {
        account_id: jwt_account_id(&access),
        access,
        refresh,
    })
}

fn refresh_credential(previous: &Credential) -> Option<Credential> {
    // Try the Pi CLI refresh first (most likely to have a working session),
    // then the Codex CLI OAuth refresh.
    refreshed_pi_credential(previous).or_else(|| refreshed_codex_credential(previous))
}

fn request_usage(credential: &Credential) -> Result<(Value, Option<f64>, Option<f64>), u16> {
    let mut request = ureq::get(CODEX_USAGE_URL)
        .timeout(Duration::from_secs(15))
        .set("Authorization", &format!("Bearer {}", credential.access))
        .set("Accept", "application/json")
        .set("User-Agent", "Tokenscope");
    if !credential.account_id.is_empty() {
        request = request.set("ChatGPT-Account-Id", &credential.account_id);
    }
    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(status, _)) => return Err(status),
        Err(_) => return Err(0),
    };
    let primary = response
        .header("X-Codex-Primary-Used-Percent")
        .and_then(|value| value.parse().ok());
    let secondary = response
        .header("X-Codex-Secondary-Used-Percent")
        .and_then(|value| value.parse().ok());
    response
        .into_json::<Value>()
        .map(|body| (body, primary, secondary))
        .map_err(|_| 0)
}

fn native_number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(number) => number.parse().ok(),
        _ => None,
    }
}

/// Native wham/usage decoding → the same ProviderLimit shape as `hu`'s Codex
/// envelope: primary pool becomes Weekly, `codex_bengalfox` becomes Spark.
fn codex_from_native() -> Option<ProviderLimit> {
    let credential = read_pi_credential().or_else(read_codex_credential)?;
    let (body, primary_header, _) = request_usage(&credential)
        .or_else(|status| {
            if status != 401 {
                return Err(status);
            }
            let refreshed = refresh_credential(&credential).ok_or(status)?;
            request_usage(&refreshed)
        })
        .ok()?;
    let plan = body.get("plan_type").and_then(Value::as_str).unwrap_or("");
    let now_ms = now_ms();
    let mut windows = Vec::new();
    if let Some(rate_limit) = body.get("rate_limit") {
        if let Some(primary) = rate_limit.get("primary_window") {
            let used = primary_header
                .or_else(|| native_number(primary.get("used_percent")))
                .unwrap_or(0.0);
            let resets_at = primary
                .get("reset_at")
                .and_then(|value| value.as_i64())
                .or_else(|| {
                    native_number(primary.get("reset_after_seconds"))
                        .map(|seconds| now_ms / 1000 + seconds as i64)
                })
                .unwrap_or(0);
            windows.push(LimitWindow {
                id: "weekly".to_string(),
                label: "Weekly".to_string(),
                duration_minutes: WEEKLY_WINDOW_MINUTES,
                used_pct: used,
                resets_at,
                as_of_ms: now_ms,
                trend: Vec::new(),
            });
        }
    }
    if let Some(limits) = body
        .get("additional_rate_limits")
        .and_then(Value::as_array)
    {
        for entry in limits {
            let is_spark = entry
                .get("metered_feature")
                .and_then(Value::as_str)
                == Some("codex_bengalfox")
                || entry
                    .get("limit_name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.to_ascii_lowercase().contains("spark"));
            if !is_spark {
                continue;
            }
            if let Some(rate_limit) = entry.get("rate_limit") {
                if let Some(primary) = rate_limit.get("primary_window") {
                    let used = native_number(primary.get("used_percent")).unwrap_or(0.0);
                    let resets_at = primary
                        .get("reset_at")
                        .and_then(|value| value.as_i64())
                        .or_else(|| {
                            native_number(primary.get("reset_after_seconds"))
                                .map(|seconds| now_ms / 1000 + seconds as i64)
                        })
                        .unwrap_or(0);
                    windows.push(LimitWindow {
                        id: "spark".to_string(),
                        label: "Spark".to_string(),
                        duration_minutes: WEEKLY_WINDOW_MINUTES,
                        used_pct: used,
                        resets_at,
                        as_of_ms: now_ms,
                        trend: Vec::new(),
                    });
                }
            }
        }
    }
    if windows.is_empty() {
        return None;
    }
    Some(ProviderLimit {
        provider: "codex".to_string(),
        label: "Codex".to_string(),
        plan: plan.to_string(),
        windows,
    })
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
