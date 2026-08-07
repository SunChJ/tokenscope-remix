mod codex_adapter;
mod config;
mod model;
mod parser;
mod pricing;
mod quota_api;
mod store;

use model::{Dashboard, RangeDashboard};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(not(target_os = "macos"))]
use tauri::WindowEvent;
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager,
};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
// Positioner is only used for the non-macOS fallback; macOS positions the
// NSPanel manually (see position_panel).
#[cfg(not(target_os = "macos"))]
use tauri_plugin_positioner::{Position, WindowExt};
// NSPanel: lets the popover float over apps in native fullscreen (a plain
// NSWindow from a background/Accessory app cannot overlay another app's
// fullscreen Space). `get_webview_panel` / `to_panel` come from these traits.
#[cfg(target_os = "macos")]
use tauri_nspanel::{ManagerExt as _, WebviewWindowExt as _};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

const DASHBOARD_SHORTCUT: &str = "CommandOrControl+Alt+T";

/// What the menu-bar title shows next to today's token count. Compact shows
/// the tightest window per provider; Detailed lists every window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum MenuBarQuotaDisplay {
    #[default]
    Off,
    Compact,
    Detailed,
}

impl<'de> serde::Deserialize<'de> for MenuBarQuotaDisplay {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "off" => Self::Off,
            // Legacy weekly-remaining values map to the compact summary.
            "codex" | "codex_and_spark" => Self::Compact,
            "compact" => Self::Compact,
            "detailed" => Self::Detailed,
            _ => Self::Off,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum AppLanguage {
    #[default]
    En,
    Zh,
}

impl AppLanguage {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "en" => Some(Self::En),
            "zh" => Some(Self::Zh),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Zh => "zh",
        }
    }
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
struct TrayPreferences {
    // Persisted key stays `weekly_quota_display` for forward compatibility;
    // the semantics are now Menu Bar Display (off / compact / detailed).
    weekly_quota_display: MenuBarQuotaDisplay,
    dashboard_shortcut: bool,
    dashboard_shortcut_key: String,
    language: AppLanguage,
}

impl Default for TrayPreferences {
    fn default() -> Self {
        Self {
            weekly_quota_display: MenuBarQuotaDisplay::Off,
            dashboard_shortcut: false,
            dashboard_shortcut_key: DASHBOARD_SHORTCUT.to_string(),
            language: AppLanguage::En,
        }
    }
}

struct TrayPreferencesState(std::sync::Mutex<TrayPreferences>);
struct TrayMenuState {
    open: MenuItem<tauri::Wry>,
    refresh: MenuItem<tauri::Wry>,
    check_updates: MenuItem<tauri::Wry>,
    provider_limits: Submenu<tauri::Wry>,
    provider_claude: MenuItem<tauri::Wry>,
    provider_codex: MenuItem<tauri::Wry>,
    menu_bar_display: Submenu<tauri::Wry>,
    display_off: CheckMenuItem<tauri::Wry>,
    display_compact: CheckMenuItem<tauri::Wry>,
    display_detailed: CheckMenuItem<tauri::Wry>,
    dashboard_shortcut: CheckMenuItem<tauri::Wry>,
    change_dashboard_shortcut: MenuItem<tauri::Wry>,
    autostart: CheckMenuItem<tauri::Wry>,
    language: Submenu<tauri::Wry>,
    language_en: CheckMenuItem<tauri::Wry>,
    language_zh: CheckMenuItem<tauri::Wry>,
    quit: MenuItem<tauri::Wry>,
}

struct TrayCopy {
    open: &'static str,
    refresh: &'static str,
    check_updates: &'static str,
    provider_limits: &'static str,
    provider_unavailable: &'static str,
    menu_bar_display: &'static str,
    off: &'static str,
    compact: &'static str,
    detailed: &'static str,
    dashboard_shortcut: &'static str,
    change_dashboard_shortcut: &'static str,
    autostart: &'static str,
    language: &'static str,
    english: &'static str,
    chinese: &'static str,
    quit: &'static str,
    today: &'static str,
    ready: &'static str,
}

fn tray_copy(language: AppLanguage) -> &'static TrayCopy {
    static EN: TrayCopy = TrayCopy {
        open: "Open Tokenscope",
        refresh: "Refresh",
        check_updates: "Check for Updates…",
        provider_limits: "Provider Limits",
        provider_unavailable: "Unavailable",
        menu_bar_display: "Menu Bar Display",
        off: "Off",
        compact: "Compact",
        detailed: "Detailed",
        dashboard_shortcut: "Dashboard Shortcut",
        change_dashboard_shortcut: "Change Dashboard Shortcut…",
        autostart: "Launch at Login",
        language: "Language",
        english: "English",
        chinese: "Simplified Chinese",
        quit: "Quit",
        today: "today",
        ready: "Ready",
    };
    static ZH: TrayCopy = TrayCopy {
        open: "打开 Tokenscope",
        refresh: "刷新",
        check_updates: "检查更新…",
        provider_limits: "额度详情",
        provider_unavailable: "不可用",
        menu_bar_display: "菜单栏显示",
        off: "关闭",
        compact: "紧凑",
        detailed: "详细",
        dashboard_shortcut: "Dashboard 快捷键",
        change_dashboard_shortcut: "修改 Dashboard 快捷键…",
        autostart: "登录时启动",
        language: "语言",
        english: "English",
        chinese: "简体中文",
        quit: "退出",
        today: "今日",
        ready: "就绪",
    };
    match language {
        AppLanguage::En => &EN,
        AppLanguage::Zh => &ZH,
    }
}

fn shortcut_label(shortcut: &str) -> String {
    let macos = cfg!(target_os = "macos");
    let mut parts = Vec::new();
    for part in shortcut.split('+') {
        let (order, label) = match part.to_ascii_uppercase().as_str() {
            "COMMANDORCONTROL" | "COMMANDORCTRL" | "CMDORCTRL" | "CMDORCONTROL" => {
                if macos {
                    (4, "⌘")
                } else {
                    (1, "Ctrl")
                }
            }
            "COMMAND" | "CMD" | "SUPER" => (4, if macos { "⌘" } else { "Win" }),
            "CONTROL" | "CTRL" => (1, if macos { "⌃" } else { "Ctrl" }),
            "OPTION" | "ALT" => (2, if macos { "⌥" } else { "Alt" }),
            "SHIFT" => (3, if macos { "⇧" } else { "Shift" }),
            _ => (
                5,
                part.strip_prefix("Key")
                    .or_else(|| part.strip_prefix("Digit"))
                    .unwrap_or(part),
            ),
        };
        parts.push((order, label));
    }
    parts.sort_by_key(|part| part.0);
    let labels = parts.into_iter().map(|part| part.1).collect::<Vec<_>>();
    if macos {
        labels.join("")
    } else {
        labels.join("+")
    }
}

fn dashboard_shortcut_menu_label(shortcut: &str, language: AppLanguage) -> String {
    format!(
        "{} ({})",
        tray_copy(language).dashboard_shortcut,
        shortcut_label(shortcut)
    )
}

fn apply_tray_language(menu: &TrayMenuState, language: AppLanguage, shortcut: &str) {
    let copy = tray_copy(language);
    let _ = menu.open.set_text(copy.open);
    let _ = menu.refresh.set_text(copy.refresh);
    let _ = menu.check_updates.set_text(copy.check_updates);
    let _ = menu.provider_limits.set_text(copy.provider_limits);
    let _ = menu.menu_bar_display.set_text(copy.menu_bar_display);
    let _ = menu.display_off.set_text(copy.off);
    let _ = menu.display_compact.set_text(copy.compact);
    let _ = menu.display_detailed.set_text(copy.detailed);
    let _ = menu
        .dashboard_shortcut
        .set_text(dashboard_shortcut_menu_label(shortcut, language));
    let _ = menu
        .change_dashboard_shortcut
        .set_text(copy.change_dashboard_shortcut);
    let _ = menu.autostart.set_text(copy.autostart);
    let _ = menu.language.set_text(copy.language);
    let _ = menu.language_en.set_text(copy.english);
    let _ = menu.language_zh.set_text(copy.chinese);
    let _ = menu.language_en.set_checked(language == AppLanguage::En);
    let _ = menu.language_zh.set_checked(language == AppLanguage::Zh);
    let _ = menu.quit.set_text(copy.quit);
}

/// Percentage left for a window, hidden once its reset timestamp has passed
/// (a dormant pool may stop emitting snapshots after the window rolls over).
fn window_left(window: &model::LimitWindow, now_s: i64) -> Option<u8> {
    if window.resets_at > 0 && window.resets_at <= now_s {
        return None;
    }
    Some((100.0 - window.used_pct).clamp(0.0, 100.0).round() as u8)
}

/// Provider prefix + window token used in menu-bar summaries.
/// Claude → Cl, Codex → Cx; windows: 5h / W / S.
fn provider_prefix(provider: &str) -> &'static str {
    match provider {
        "claude" => "Cl",
        "codex" => "Cx",
        _ => "",
    }
}

fn window_token(window: &model::LimitWindow) -> &'static str {
    match window.id.as_str() {
        "5h" => "5h",
        "spark" => "S",
        _ => "W",
    }
}

/// One provider's menu-bar segment: compact shows only the tightest window,
/// detailed lists every active window (`Cl 5h64/W82`).
fn provider_summary(limit: &model::ProviderLimit, detailed: bool, now_s: i64) -> String {
    let prefix = provider_prefix(&limit.provider);
    let active: Vec<&model::LimitWindow> = limit
        .windows
        .iter()
        .filter(|window| window_left(window, now_s).is_some())
        .collect();
    if active.is_empty() {
        return String::new();
    }
    if !detailed {
        let tightest = limit
            .windows
            .iter()
            .filter(|window| window_left(window, now_s).is_some())
            .max_by(|a, b| {
                a.used_pct
                    .partial_cmp(&b.used_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .expect("non-empty");
        let left = window_left(tightest, now_s).expect("filtered");
        return format!("{prefix}{left}%");
    }
    let mut sorted = active;
    sorted.sort_by_key(|window| match window.id.as_str() {
        "5h" => 0,
        "weekly" => 1,
        _ => 2,
    });
    let parts: Vec<String> = sorted
        .iter()
        .filter_map(|window| {
            window_left(window, now_s).map(|left| {
                format!("{}{}%", window_token(window), left)
            })
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("{prefix} {}", parts.join("/"))
    }
}

fn tray_label(dash: &Dashboard, display: MenuBarQuotaDisplay, language: AppLanguage) -> String {
    let mut label = fmt_tokens_m(dash.today_tokens, language);
    let now_s = now_ms() / 1000;
    match display {
        MenuBarQuotaDisplay::Off => {}
        MenuBarQuotaDisplay::Compact | MenuBarQuotaDisplay::Detailed => {
            let detailed = display == MenuBarQuotaDisplay::Detailed;
            let mut segments: Vec<String> = dash
                .provider_limits
                .iter()
                .map(|limit| provider_summary(limit, detailed, now_s))
                .filter(|segment| !segment.is_empty())
                .collect();
            if !segments.is_empty() {
                segments.insert(0, label);
                label = segments.join(" · ");
            }
        }
    }
    label
}

/// Menu-bar text for each provider row under Provider Limits, e.g.
/// "Claude — 5h 64% · W 82%" (Claude) or "Codex — W 29% · S 31%" (Codex).
fn provider_menu_row(
    limit: Option<&model::ProviderLimit>,
    provider_label: &str,
    unavailable: &str,
    now_s: i64,
) -> String {
    let Some(limit) = limit else {
        // Empty native menu-item titles collapse the macOS submenu into the
        // tiny blank scroller seen when `hu` cannot run.
        return format!("{provider_label} — {unavailable}");
    };
    let mut sorted: Vec<&model::LimitWindow> = limit
        .windows
        .iter()
        .filter(|window| window_left(window, now_s).is_some())
        .collect();
    sorted.sort_by_key(|window| match window.id.as_str() {
        "5h" => 0,
        "weekly" => 1,
        _ => 2,
    });
    let parts: Vec<String> = sorted
        .iter()
        .filter_map(|window| {
            window_left(window, now_s).map(|left| {
                let label = match window.id.as_str() {
                    "5h" => "5h".to_string(),
                    "weekly" => "W".to_string(),
                    _ => "S".to_string(),
                };
                format!("{label} {left}%")
            })
        })
        .collect();
    if parts.is_empty() {
        return format!("{} — —", limit.label);
    }
    format!("{} — {}", limit.label, parts.join(" · "))
}

fn update_tray_label(app: &tauri::AppHandle, dash: &Dashboard) {
    let (display, language) = app
        .try_state::<TrayPreferencesState>()
        .map(|state| {
            state
                .0
                .lock()
                .map(|prefs| (prefs.weekly_quota_display, prefs.language))
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let label = tray_label(dash, display, language);
    let today = tray_copy(language).today;
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(tray) = handle.tray_by_id("main") {
            // macOS shows the label next to the menu-bar icon (set_title).
            // Windows' taskbar tray has no equivalent, so mirror it into the
            // tooltip there. Both APIs touch native tray state; on macOS that
            // must happen on the main thread.
            let _ = tray.set_title(Some(label.clone()));
            let _ = tray.set_tooltip(Some(format!("Tokenscope · {today} {label}")));
        }
    });
}

/// Rebuild the dashboard (incremental), update the tray's token count and the
/// Provider Limits menu rows, then push the fresh data to the UI so an open
/// popover updates live.
fn refresh(app: &tauri::AppHandle) {
    let dash = parser::build_dashboard();
    update_tray_label(app, &dash);
    update_provider_rows(app, &dash);
    let _ = app.emit("dashboard-updated", &dash);
}

/// Keep the two Provider Limits menu rows in sync with the latest snapshot.
fn update_provider_rows(app: &tauri::AppHandle, dash: &Dashboard) {
    let Some(state) = app.try_state::<TrayMenuState>() else {
        return;
    };
    let now_s = now_ms() / 1000;
    let (claude, codex) = provider_rows(&dash.provider_limits);
    let language = app
        .try_state::<TrayPreferencesState>()
        .and_then(|state| state.0.lock().ok().map(|prefs| prefs.language))
        .unwrap_or_default();
    let copy = tray_copy(language);
    let claude_text = provider_menu_row(claude, "Claude", copy.provider_unavailable, now_s);
    let codex_text = provider_menu_row(codex, "Codex", copy.provider_unavailable, now_s);
    let provider_claude = state.provider_claude.clone();
    let provider_codex = state.provider_codex.clone();
    let handle = app.clone();
    let _ = handle.run_on_main_thread(move || {
        let _ = provider_claude.set_text(claude_text);
        let _ = provider_codex.set_text(codex_text);
    });
}

fn provider_rows(
    limits: &[model::ProviderLimit],
) -> (Option<&model::ProviderLimit>, Option<&model::ProviderLimit>) {
    let mut claude = None;
    let mut codex = None;
    for limit in limits {
        match limit.provider.as_str() {
            "claude" => claude = Some(limit),
            "codex" => codex = Some(limit),
            _ => {}
        }
    }
    (claude, codex)
}

/// Cooldown for manual force-refreshes (the tray "Refresh" item). Price tables
/// change at most a few times a day, so back-to-back clicks inside this window
/// coalesce into one fetch.
const FORCE_COOLDOWN_MS: i64 = 30_000;
static LAST_FORCE_MS: AtomicI64 = AtomicI64::new(0);

/// Off-thread, silent price-table refresh (models.dev + LiteLLM) bypassing the
/// 24h cache, folded into the tray's "Refresh" item. Returns immediately; once
/// the new table is swapped in, refresh() pushes dashboard-updated so an open
/// panel re-prices live, same silent path as the 30s background poll (no
/// loading state, no UI feedback). Throttled to one per FORCE_COOLDOWN_MS via
/// compare_exchange (fixed window, not sliding) so rapid clicks can't spawn
/// concurrent fetches racing on the cache.
fn refresh_pricing_bg(app: &tauri::AppHandle) {
    let now = now_ms();
    loop {
        let prev = LAST_FORCE_MS.load(Ordering::Relaxed);
        if now - prev < FORCE_COOLDOWN_MS {
            return;
        }
        match LAST_FORCE_MS.compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(_) => continue,
        }
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        pricing::Pricing::reload_shared(true);
        refresh(&handle);
    });
}

#[tauri::command]
fn refresh_pricing(app: tauri::AppHandle) {
    refresh_pricing_bg(&app);
}

// ── Launch-at-login preference ──────────────────────────────────────
// Persisted in the data dir so it survives restarts and updates. The
// on/off toggle lives in the tray's right-click menu; on startup we reconcile
// the OS registration to this preference rather than force-enabling every
// launch (which silently undid a user who had turned autostart off).
fn autostart_pref_path() -> Option<std::path::PathBuf> {
    let dir = dirs::data_dir()?.join("tokenscope");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("autostart.json"))
}

fn load_autostart_pref() -> Option<bool> {
    let t = std::fs::read_to_string(autostart_pref_path()?).ok()?;
    serde_json::from_str(&t).ok()
}

fn save_autostart_pref(on: bool) {
    if let Some(p) = autostart_pref_path() {
        if let Ok(t) = serde_json::to_string(&on) {
            let _ = std::fs::write(p, t);
        }
    }
}

fn tray_preferences_path() -> Option<std::path::PathBuf> {
    let dir = dirs::data_dir()?.join("tokenscope");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("tray.json"))
}

fn load_tray_preferences() -> TrayPreferences {
    let Some(path) = tray_preferences_path() else {
        return TrayPreferences::default();
    };
    let Some(text) = std::fs::read_to_string(path).ok() else {
        return TrayPreferences::default();
    };
    parse_tray_preferences(&text)
}

fn parse_tray_preferences(text: &str) -> TrayPreferences {
    let mut preferences: TrayPreferences = serde_json::from_str(text).unwrap_or_default();
    let legacy_weekly_on = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .filter(|value| value.get("weekly_quota_display").is_none())
        .and_then(|value| value.get("show_weekly_remaining")?.as_bool())
        .unwrap_or(false);
    if legacy_weekly_on {
        preferences.weekly_quota_display = MenuBarQuotaDisplay::Compact;
    }
    preferences
}

fn save_tray_preferences(preferences: &TrayPreferences) {
    if let Some(path) = tray_preferences_path() {
        if let Ok(text) = serde_json::to_string(preferences) {
            let _ = std::fs::write(path, text);
        }
    }
}

/// Bring the OS launch-at-login registration in line with the saved preference,
/// returning the effective preference (used to seed the menu checkbox). First
/// run (no saved pref) defaults to on and records it; thereafter we honor the
/// user's choice and only touch the registration when it actually differs.
fn reconcile_autostart(app: &tauri::AppHandle) -> bool {
    let pref = match load_autostart_pref() {
        Some(p) => p,
        None => {
            save_autostart_pref(true);
            true
        }
    };
    let mgr = app.autolaunch();
    let cur = mgr.is_enabled().unwrap_or(false);
    if pref && !cur {
        let _ = mgr.enable();
    } else if !pref && cur {
        let _ = mgr.disable();
    }
    pref
}

/// Last tray-icon rectangle (physical px: x, y, width, height), captured on tray
/// click. Used to anchor the panel like tauri-plugin-positioner's
/// TrayBottomCenter — but we can't use the positioner itself on a swizzled
/// NSPanel: its calculate_position calls current_monitor().unwrap(), which fails
/// for a hidden/panel window, so positioning silently no-ops (panel stays
/// top-left). We also must add the icon height ourselves (see position_panel).
///
/// On Windows the cached tray rect is used only to pick which monitor the
/// popover opens on (see position_popover_windows); the popover itself is then
/// pinned to that monitor's top-right work-area corner with a small margin.
struct TrayAnchor(std::sync::Mutex<Option<(f64, f64, f64, f64)>>);

/// Timestamp (ms) of the last drag start. The popover hides on focus loss; on
/// Windows `start_dragging` enters the OS move loop which briefly blurs the
/// window, so we ignore the hide for a short window after a drag.
#[cfg(not(target_os = "macos"))]
struct DragGuard(AtomicI64);

/// Start dragging the borderless popover (Windows/Linux). Done via a command
/// (not the JS drag-region) so we can record the drag start and suppress the
/// imminent hide-on-blur. The frontend only calls this once a real drag begins.
#[cfg(not(target_os = "macos"))]
#[tauri::command]
fn begin_drag(window: tauri::Window) -> Result<(), String> {
    if let Some(g) = window.try_state::<DragGuard>() {
        g.0.store(now_ms(), Ordering::Relaxed);
    }
    window.start_dragging().map_err(|e| e.to_string())
}

/// macOS uses a menu-bar NSPanel that isn't user-draggable, so begin_drag is a
/// no-op there. It's also never invoked (the frontend gates it out) — this just
/// keeps the shared invoke_handler list valid and guarantees zero macOS effect.
#[cfg(target_os = "macos")]
#[tauri::command]
fn begin_drag(_window: tauri::Window) -> Result<(), String> {
    Ok(())
}

/// Anchor the panel under the tray icon, top flush with the menu-bar bottom:
///   x = tray_x + tray_width/2 − window_width/2
///   y = tray_y + tray_height
/// The tray rect's y is the icon *top* (≈ screen top, 0); adding its height
/// lands the panel just below the menu bar. (tauri-plugin-positioner gets away
/// with y = tray_y because macOS auto-constrains a normal window out from under
/// the menu bar — but a floating NSPanel isn't constrained, so we offset it
/// ourselves.) All physical px; no monitor lookup, so it works while hidden.
#[cfg(target_os = "macos")]
fn position_panel(app: &tauri::AppHandle) {
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    let Ok(size) = w.outer_size() else {
        return;
    };
    let win_w = size.width as f64;

    if let Some(state) = app.try_state::<TrayAnchor>() {
        if let Some((tx, ty, tw, th)) = *state.0.lock().unwrap() {
            let x = tx + tw / 2.0 - win_w / 2.0;
            let y = ty + th;
            let _ = w.set_position(tauri::PhysicalPosition::new(x as i32, y as i32));
            return;
        }
    }

    // Fallback (e.g. opened from the menu before any tray click): centre near
    // the top of the current monitor.
    if let Ok(Some(monitor)) = w.current_monitor() {
        let mp = monitor.position();
        let ms = monitor.size();
        let x = mp.x as f64 + (ms.width as f64 - win_w) / 2.0;
        let y = mp.y as f64 + 24.0 * monitor.scale_factor();
        let _ = w.set_position(tauri::PhysicalPosition::new(x as i32, y as i32));
    }
}

// ── Popover position memory (Windows/Linux) ─────────────────────────
// The borderless popover can be dragged (a header drag region calls
// startDragging in the frontend); we remember where the user left it and reopen
// there next time, falling back to the default top-right when there's no saved
// position on a connected monitor. macOS uses a menu-bar-anchored NSPanel and
// does not persist a position.
#[cfg(not(target_os = "macos"))]
fn popover_pos_path() -> Option<std::path::PathBuf> {
    let dir = dirs::data_dir()?.join("tokenscope");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("popover_pos.json"))
}

#[cfg(not(target_os = "macos"))]
fn load_popover_pos() -> Option<(i32, i32)> {
    let t = std::fs::read_to_string(popover_pos_path()?).ok()?;
    serde_json::from_str(&t).ok()
}

#[cfg(not(target_os = "macos"))]
fn save_popover_pos(x: i32, y: i32) {
    if let Some(p) = popover_pos_path() {
        if let Ok(t) = serde_json::to_string(&(x, y)) {
            let _ = std::fs::write(p, t);
        }
    }
}

/// Position AND right-size the popover for the monitor it opens on. Reopens at
/// the user's last-dragged spot if it's still on a connected monitor, else pins
/// to the top-right of the tray monitor's work area (margin from the edges).
///
/// Everything is derived from the *intended* logical size × the target monitor's
/// scale — never the window's current physical size — and the size is re-asserted
/// on every open. A borderless window can otherwise get stuck at the previous
/// monitor's physical size after a DPI/monitor change (e.g. unplugging a 175%
/// display drops back to 100% but the window stays oversized until restart);
/// forcing the size here makes it recover on the next open. The monitor is
/// resolved from the cached tray rect -> current -> primary; work_area excludes
/// the taskbar so the margin is clean wherever the taskbar sits.
#[cfg(not(target_os = "macos"))]
fn position_popover_windows(app: &tauri::AppHandle) {
    // Logical size — must match app.windows[0] width/height in tauri.conf.json.
    const POPOVER_W: f64 = 400.0;
    const POPOVER_H: f64 = 660.0;
    const MARGIN: f64 = 12.0; // logical px gap from the screen edges

    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    // Force the intended size at the target monitor's DPI (recovers a stuck size).
    let fit = |scale: f64| {
        let _ = w.set_size(tauri::PhysicalSize::new(
            (POPOVER_W * scale).round() as u32,
            (POPOVER_H * scale).round() as u32,
        ));
    };

    // 1. Reopen at the last position if a point just inside it is still on a
    //    connected monitor (a disconnected/shrunk monitor falls through to the
    //    default rather than opening off-screen).
    if let Some((sx, sy)) = load_popover_pos() {
        if let Ok(Some(m)) = w.monitor_from_point(sx as f64 + 20.0, sy as f64 + 20.0) {
            let _ = w.set_position(tauri::PhysicalPosition::new(sx, sy));
            fit(m.scale_factor());
            return;
        }
    }

    // 2. Default: top-right of the tray monitor's work area.
    //    Prefer the monitor under the tray icon; fall back to current, then primary.
    let anchor = app
        .try_state::<TrayAnchor>()
        .and_then(|s| *s.0.lock().unwrap());
    let monitor = anchor
        .and_then(|(tx, ty, _, _)| w.monitor_from_point(tx, ty).ok().flatten())
        .or_else(|| w.current_monitor().ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());

    if let Some(m) = monitor {
        let area = m.work_area(); // excludes the taskbar
        let scale = m.scale_factor();
        let margin = MARGIN * scale; // keep the visual gap DPI-consistent
        let win_w = POPOVER_W * scale; // intended physical width on this monitor
        let right = area.position.x as f64 + area.size.width as f64;
        let x = right - win_w - margin;
        let y = area.position.y as f64 + margin;
        let _ = w.set_position(tauri::PhysicalPosition::new(x as i32, y as i32));
        fit(scale);
    } else {
        // Couldn't resolve a monitor (rare) → let the positioner place it.
        let _ = w.move_window(Position::TopRight);
    }
}

/// True if our (Accessory) app is currently the frontmost application.
#[cfg(target_os = "macos")]
#[allow(deprecated)] // tauri_nspanel::cocoa (objc2 migration is upstream's)
fn app_is_frontmost() -> bool {
    use tauri_nspanel::cocoa::base::id;
    use tauri_nspanel::objc::{class, msg_send, sel, sel_impl};
    unsafe {
        let proc_info: id = msg_send![class!(NSProcessInfo), processInfo];
        let our_pid: i32 = msg_send![proc_info, processIdentifier];
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let front: id = msg_send![workspace, frontmostApplication];
        if front.is_null() {
            return false;
        }
        let front_pid: i32 = msg_send![front, processIdentifier];
        front_pid == our_pid
    }
}

/// Hide the panel when the user switches Space or activates another app, so it
/// doesn't linger over the new (e.g. fullscreen) Space until the next click.
/// resign-key alone misses pure Space switches because the panel joins all
/// Spaces and can stay key across the transition.
#[cfg(target_os = "macos")]
fn hide_panel_on_context_switch(app: &tauri::AppHandle) {
    if app_is_frontmost() {
        return;
    }
    if let Ok(panel) = app.get_webview_panel("main") {
        if panel.is_visible() {
            panel.order_out(None);
        }
    }
}

/// Register NSWorkspace observers that auto-hide the panel on Space change / app
/// activation (mirrors tauri-nspanel's menu-bar example). The observers live for
/// the whole app lifetime, so the returned tokens are intentionally dropped.
#[cfg(target_os = "macos")]
#[allow(deprecated)] // tauri_nspanel::cocoa (objc2 migration is upstream's)
fn register_panel_autohide(app: &tauri::AppHandle) {
    use std::ffi::CString;
    use tauri_nspanel::block::ConcreteBlock;
    use tauri_nspanel::cocoa::base::{id, nil};
    use tauri_nspanel::objc::{class, msg_send, sel, sel_impl};

    unsafe {
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let center: id = msg_send![workspace, notificationCenter];
        for name in [
            "NSWorkspaceActiveSpaceDidChangeNotification",
            "NSWorkspaceDidActivateApplicationNotification",
        ] {
            let app = app.clone();
            let block = ConcreteBlock::new(move |_notif: id| {
                hide_panel_on_context_switch(&app);
            });
            let block = block.copy();
            let ns_name: id = msg_send![
                class!(NSString),
                stringWithUTF8String: CString::new(name).unwrap().as_ptr()
            ];
            let _: id = msg_send![
                center,
                addObserverForName: ns_name object: nil queue: nil usingBlock: block
            ];
        }
    }
}

/// Read the user's GLOBAL macOS appearance preference: true when dark mode is on.
/// We read `AppleInterfaceStyle` from NSUserDefaults (present and "Dark" => dark,
/// absent => light) rather than the app's NSApp.effectiveAppearance — an
/// Accessory (menu-bar) app never becomes frontmost, so its effective appearance
/// (and thus the webview's `prefers-color-scheme`) can lag the real system value.
/// The user default reflects the system setting directly, regardless of focus.
#[cfg(target_os = "macos")]
#[allow(deprecated)] // tauri_nspanel::cocoa (objc2 migration is upstream's)
fn system_is_dark() -> bool {
    use std::ffi::CStr;
    use tauri_nspanel::cocoa::base::{id, nil};
    use tauri_nspanel::objc::{class, msg_send, sel, sel_impl};
    unsafe {
        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key: id = msg_send![
            class!(NSString),
            stringWithUTF8String: c"AppleInterfaceStyle".as_ptr()
        ];
        let val: id = msg_send![defaults, stringForKey: key];
        if val == nil {
            return false;
        }
        let raw: *const std::os::raw::c_char = msg_send![val, UTF8String];
        if raw.is_null() {
            return false;
        }
        CStr::from_ptr(raw).to_string_lossy().eq_ignore_ascii_case("dark")
    }
}

/// Watch for live system dark/light-mode changes and push them to the frontend.
/// `AppleInterfaceThemeChangedNotification` is posted on the DISTRIBUTED
/// notification center the instant the user flips Appearance, and is delivered
/// to every registered app regardless of activation policy or frontmost status —
/// so it works for our hidden, non-activating menu-bar panel where the webview's
/// own `prefers-color-scheme` `change` event does not reliably fire. The observer
/// lives for the whole app lifetime, so the returned token is intentionally
/// dropped (same as register_panel_autohide).
#[cfg(target_os = "macos")]
#[allow(deprecated)] // tauri_nspanel::cocoa (objc2 migration is upstream's)
fn watch_system_theme(app: &tauri::AppHandle) {
    use std::ffi::CString;
    use tauri_nspanel::block::ConcreteBlock;
    use tauri_nspanel::cocoa::base::{id, nil};
    use tauri_nspanel::objc::{class, msg_send, sel, sel_impl};

    unsafe {
        let center: id = msg_send![class!(NSDistributedNotificationCenter), defaultCenter];
        let app = app.clone();
        let block = ConcreteBlock::new(move |_notif: id| {
            let _ = app.emit("system-theme", system_is_dark());
        });
        let block = block.copy();
        let ns_name: id = msg_send![
            class!(NSString),
            stringWithUTF8String: CString::new("AppleInterfaceThemeChangedNotification").unwrap().as_ptr()
        ];
        let _: id = msg_send![
            center,
            addObserverForName: ns_name object: nil queue: nil usingBlock: block
        ];
    }
}

/// Show the panel as a popover anchored under the tray icon, and focus it.
/// Always reset the scroll to the top so it doesn't reopen mid-scroll.
fn show_popover(app: &tauri::AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || show_popover_inner(&handle));
}

fn toggle_popover(app: &tauri::AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        #[cfg(target_os = "macos")]
        {
            let visible = handle
                .get_webview_panel("main")
                .map(|panel| panel.is_visible())
                .unwrap_or_else(|_| {
                    handle
                        .get_webview_window("main")
                        .and_then(|window| window.is_visible().ok())
                        .unwrap_or(false)
                });
            if visible {
                if let Ok(panel) = handle.get_webview_panel("main") {
                    panel.order_out(None);
                } else if let Some(window) = handle.get_webview_window("main") {
                    let _ = window.hide();
                }
            } else {
                show_popover_inner(&handle);
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let visible = handle
                .get_webview_window("main")
                .and_then(|window| window.is_visible().ok())
                .unwrap_or(false);
            if visible {
                if let Some(window) = handle.get_webview_window("main") {
                    let _ = window.hide();
                }
            } else {
                show_popover_inner(&handle);
            }
        }
    });
}

fn show_popover_inner(app: &tauri::AppHandle) {
    // On macOS the window is an NSPanel — position it manually, then show()
    // (makes it key and orders it front, incl. over fullscreen Spaces).
    #[cfg(target_os = "macos")]
    {
        position_panel(app);
        match app.get_webview_panel("main") {
            Ok(panel) => panel.show(),
            Err(_) => {
                // If the panel state is ever unavailable (for example after a
                // plugin/setup ordering regression), still surface the window
                // instead of making a tray click look like a no-op.
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    if let Some(w) = app.get_webview_window("main") {
        // Pin the popover to the monitor's top-right corner (see
        // position_popover_windows).
        position_popover_windows(app);
        let _ = w.show();
        let _ = w.set_focus();
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.eval(
            "(function(){var e=document.querySelector('.om-scroll');if(e){e.scrollTop=0;}else{window.scrollTo(0,0);}})()",
        );
    }
}

#[tauri::command]
fn set_dashboard_shortcut(app: tauri::AppHandle, shortcut: String) -> Result<(), String> {
    let shortcut = shortcut.trim().to_string();
    shortcut
        .parse::<Shortcut>()
        .map_err(|_| "invalid shortcut".to_string())?;

    let state = app
        .try_state::<TrayPreferencesState>()
        .ok_or_else(|| "shortcut state unavailable".to_string())?;
    let (was_enabled, previous) = state
        .0
        .lock()
        .map(|preferences| {
            (
                preferences.dashboard_shortcut,
                preferences.dashboard_shortcut_key.clone(),
            )
        })
        .map_err(|_| "shortcut state unavailable".to_string())?;

    if was_enabled && previous != shortcut {
        app.global_shortcut()
            .unregister(previous.as_str())
            .map_err(|error| format!("could not replace shortcut: {error}"))?;
    }
    if !was_enabled || previous != shortcut {
        if let Err(error) = app.global_shortcut().register(shortcut.as_str()) {
            if was_enabled && previous != shortcut {
                let _ = app.global_shortcut().register(previous.as_str());
            }
            return Err(format!("shortcut unavailable: {error}"));
        }
    }

    let preferences = state
        .0
        .lock()
        .map(|mut preferences| {
            preferences.dashboard_shortcut = true;
            preferences.dashboard_shortcut_key = shortcut.clone();
            preferences.clone()
        })
        .map_err(|_| "shortcut state unavailable".to_string())?;
    save_tray_preferences(&preferences);
    if let Some(menu) = app.try_state::<TrayMenuState>() {
        let _ = menu.dashboard_shortcut.set_checked(true);
        let _ = menu
            .dashboard_shortcut
            .set_text(dashboard_shortcut_menu_label(
                &shortcut,
                preferences.language,
            ));
    }
    Ok(())
}

#[tauri::command]
fn get_app_language(app: tauri::AppHandle) -> String {
    app.try_state::<TrayPreferencesState>()
        .and_then(|state| {
            state
                .0
                .lock()
                .ok()
                .map(|prefs| prefs.language.as_str().to_string())
        })
        .unwrap_or_else(|| AppLanguage::En.as_str().to_string())
}

#[tauri::command]
fn set_app_language(app: tauri::AppHandle, language: String) -> Result<(), String> {
    let language =
        AppLanguage::parse(language.trim()).ok_or_else(|| "invalid language".to_string())?;
    let state = app
        .try_state::<TrayPreferencesState>()
        .ok_or_else(|| "language state unavailable".to_string())?;
    let preferences = state
        .0
        .lock()
        .map(|mut preferences| {
            preferences.language = language;
            preferences.clone()
        })
        .map_err(|_| "language state unavailable".to_string())?;
    save_tray_preferences(&preferences);
    if let Some(menu) = app.try_state::<TrayMenuState>() {
        apply_tray_language(&menu, language, &preferences.dashboard_shortcut_key);
    }
    let _ = app.emit("language-changed", language.as_str());
    Ok(())
}

#[tauri::command]
async fn get_dashboard(app: tauri::AppHandle) -> Dashboard {
    // build_dashboard does blocking IO (reads/writes the cache, parses logs) and
    // holds BUILD_LOCK — running it inline would block the command on the async
    // runtime and, with a large cache, stall the UI. Hop to a blocking worker
    // (the 30s refresh thread already runs the same work off the main thread).
    let dash = tauri::async_runtime::spawn_blocking(parser::build_dashboard)
        .await
        .unwrap_or_else(|_| parser::build_dashboard());
    // Sync the tray count to this freshly-fetched value. The panel refetches the
    // instant it opens, while the tray otherwise only refreshes every 30s — so
    // without this the two could disagree for up to 30s during heavy usage.
    update_tray_label(&app, &dash);
    dash
}

#[tauri::command]
async fn get_range_dashboard(
    start_date: String,
    end_date: String,
) -> Result<RangeDashboard, String> {
    let start = chrono::NaiveDate::parse_from_str(&start_date, "%Y-%m-%d")
        .map_err(|_| "invalid start date".to_string())?;
    let end = chrono::NaiveDate::parse_from_str(&end_date, "%Y-%m-%d")
        .map_err(|_| "invalid end date".to_string())?;
    match tauri::async_runtime::spawn_blocking(move || {
        parser::build_range_dashboard(start, end)
    })
    .await
    {
        Ok(result) => result,
        Err(error) => Err(format!("failed to build date range: {error}")),
    }
}

/// Save a full-panel screenshot (a `data:image/png;base64,...` URL captured in
/// the webview) to the user's Desktop as `Tokenscope <date> at <time>.png`.
/// DOM rasterization sidesteps macOS Screen Recording permission entirely.
/// Returns the written file path on success.
#[tauri::command]
fn save_screenshot(data_url: String) -> Result<String, String> {
    use base64::Engine;
    let body = data_url
        .strip_prefix("data:image/png;base64,")
        .ok_or_else(|| "expected a data:image/png;base64,... URL".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .map_err(|e| format!("invalid base64: {e}"))?;

    let dir = dirs::desktop_dir()
        .ok_or_else(|| "could not resolve the Desktop directory".to_string())?;
    let stamp = chrono::Local::now().format("Tokenscope %Y-%m-%d at %H.%M.%S.png");
    let path = dir.join(stamp.to_string());

    std::fs::write(&path, &bytes).map_err(|e| format!("failed to write file: {e}"))?;
    Ok(path.to_string_lossy().into_owned())
}

/// Save a project settlement export to Desktop. The frontend supplies only a
/// compact generated CSV; raw JSONL content never leaves the Rust data layer.
#[tauri::command]
fn save_project_export(csv: String, label: String) -> Result<String, String> {
    if csv.len() > 5 * 1024 * 1024 {
        return Err("export is too large".to_string());
    }
    let label: String = label
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '-' | '_'))
        .take(48)
        .collect();
    let dir = dirs::desktop_dir()
        .ok_or_else(|| "could not resolve the Desktop directory".to_string())?;
    let stamp = chrono::Local::now().format("%Y-%m-%d at %H.%M.%S");
    let name = if label.trim().is_empty() {
        format!("Tokenscope Projects {stamp}.csv")
    } else {
        format!("Tokenscope {} {stamp}.csv", label.trim())
    };
    let path = dir.join(name);
    std::fs::write(&path, csv.as_bytes())
        .map_err(|error| format!("failed to write export: {error}"))?;
    Ok(path.to_string_lossy().into_owned())
}

/// For CLI/example validation against real logs.
pub fn dashboard_json() -> String {
    // CLI/example dumps are explicit diagnostics, so fetch synchronously instead
    // of relying on the desktop app's background quota refresh loop.
    quota_api::reload();
    serde_json::to_string_pretty(&parser::build_dashboard()).unwrap_or_default()
}

fn fmt_tokens_m(m: f64, language: AppLanguage) -> String {
    if m >= 1.0 {
        format!("{:.2}M", m)
    } else {
        let k = (m * 1000.0).round() as i64;
        // no usage yet (e.g. just past midnight) — "0K" reads like "OK", so
        // show a clearer idle label instead.
        if k <= 0 {
            tray_copy(language).ready.to_string()
        } else {
            format!("{k}K")
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
#[allow(deprecated)] // tauri_nspanel::cocoa (objc2 migration is upstream's)
pub fn run() {
    // Tracks when the popover was last hidden, so a click on the tray icon
    // while it's open (which first blurs/hides it) doesn't immediately reopen.
    let last_hidden = Arc::new(AtomicI64::new(0));

    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default()
        // Must be the FIRST plugin: a second launch (e.g. reinstall/relaunch)
        // hands off to the already-running instance and exits, so the menu bar
        // never shows two icons.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_popover(app);
        }))
        .plugin(tauri_plugin_positioner::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state == ShortcutState::Pressed {
                        toggle_popover(app);
                    }
                })
                .build(),
        )
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        // In-app updates: checks the GitHub release feed (latest.json) and
        // installs signed update packages; process plugin provides the
        // relaunch after install. Both driven from the frontend UpdateBanner.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init());
    // Registers the WebviewPanelManager state used by `to_panel`/`get_webview_panel`.
    #[cfg(target_os = "macos")]
    {
        builder = builder.plugin(tauri_nspanel::init());
    }

    builder
        .invoke_handler(tauri::generate_handler![
            get_dashboard,
            get_range_dashboard,
            save_screenshot,
            save_project_export,
            begin_drag,
            set_dashboard_shortcut,
            get_app_language,
            set_app_language,
            refresh_pricing
        ])
        .setup(move |app| {
            // Point quota_api at the bundled `hu` binary (app Resources/bin/hu)
            // so provider quota works without any system install.
            quota_api::set_bundle_dir(app.path().resource_dir().ok());

            // Menu-bar–only app: use the accessory policy and explicitly hide
            // the Dock tile. The explicit visibility call also covers `tauri
            // dev`, whose unbundled debug process can briefly start as a
            // regular foreground app before setup runs.
            #[cfg(target_os = "macos")]
            {
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);
                app.set_dock_visibility(false);
            }

            // Holds the latest tray-icon rect so show_popover can anchor the panel.
            // Captured in the tray click handler on every platform — see
            // position_panel (macOS, below the icon) and position_popover_windows
            // (Windows/Linux, above the icon).
            app.manage(TrayAnchor(std::sync::Mutex::new(None)));
            // Drag-start timestamp so a drag doesn't hide the popover (non-macOS).
            #[cfg(not(target_os = "macos"))]
            app.manage(DragGuard(AtomicI64::new(0)));

            // Reconcile launch-at-login with the user's saved preference. The
            // on/off toggle lives in the tray's right-click menu (built below);
            // we do NOT force-enable on every start, which would undo a manual
            // opt-out. `autostart_on` seeds the menu checkbox.
            let autostart_on = reconcile_autostart(app.handle());
            let mut tray_preferences = load_tray_preferences();
            if tray_preferences.dashboard_shortcut
                && app
                    .global_shortcut()
                    .register(tray_preferences.dashboard_shortcut_key.as_str())
                    .is_err()
            {
                tray_preferences.dashboard_shortcut = false;
                save_tray_preferences(&tray_preferences);
            }
            let weekly_quota_display = tray_preferences.weekly_quota_display;
            let dashboard_shortcut_on = tray_preferences.dashboard_shortcut;
            let dashboard_shortcut_key = tray_preferences.dashboard_shortcut_key.clone();
            let language = tray_preferences.language;
            app.manage(TrayPreferencesState(std::sync::Mutex::new(
                tray_preferences,
            )));

            // Popover behaviour. On macOS, convert the window to a non-activating
            // NSPanel so it can float over apps in native fullscreen, and hide it
            // on resign-key (clicking outside / switching apps) like a popover.
            #[cfg(target_os = "macos")]
            if let Some(window) = app.get_webview_window("main") {
                use tauri_nspanel::cocoa::appkit::NSWindowCollectionBehavior;
                // NSWindowStyleMaskNonActivatingPanel — receive events without
                // activating (stealing focus from) the frontmost app.
                #[allow(non_upper_case_globals)]
                const NS_NONACTIVATING_PANEL: i32 = 1 << 7;

                let lh = last_hidden.clone();
                let handle = app.handle().clone();
                let delegate = tauri_nspanel::panel_delegate!(TokenscopePanelDelegate {
                    window_did_resign_key
                });
                delegate.set_listener(Box::new(move |name: String| {
                    if name == "window_did_resign_key" {
                        lh.store(now_ms(), Ordering::Relaxed);
                        if let Ok(panel) = handle.get_webview_panel("main") {
                            panel.order_out(None);
                        }
                    }
                }));

                if let Ok(panel) = window.to_panel() {
                    panel.set_level(25); // NSMainMenuWindowLevel (24) + 1
                    panel.set_style_mask(NS_NONACTIVATING_PANEL);
                    // MoveToActiveSpace: the panel relocates onto whatever Space
                    // is active *when shown* — so it appears over a fullscreen app
                    // if you open it there, but it does NOT live on every Space.
                    // (CanJoinAllSpaces + Stationary made it omnipresent and kept
                    // it painted through transitions, so it lingered/ghosted over
                    // a fullscreen Space even after order_out.) FullScreenAuxiliary
                    // is what actually permits coexisting with a fullscreen window.
                    panel.set_collection_behaviour(
                        NSWindowCollectionBehavior::NSWindowCollectionBehaviorMoveToActiveSpace
                            | NSWindowCollectionBehavior::NSWindowCollectionBehaviorFullScreenAuxiliary,
                    );
                    panel.set_delegate(delegate);
                }

                // Also hide on Space change / app activation, not just resign-key.
                register_panel_autohide(app.handle());

                // Follow the system appearance natively (the webview's
                // prefers-color-scheme is unreliable for a hidden, non-activating
                // menu-bar panel). Watch for live changes, and emit the current
                // value once now so the frontend's System mode starts correct even
                // if the webview reported a stale appearance at launch.
                watch_system_theme(app.handle());
                let _ = app.emit("system-theme", system_is_dark());
            }

            // Non-macOS: keep the plain window, hide on focus loss.
            #[cfg(not(target_os = "macos"))]
            if let Some(win) = app.get_webview_window("main") {
                let w = win.clone();
                let lh = last_hidden.clone();
                win.on_window_event(move |e| match e {
                    WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        lh.store(now_ms(), Ordering::Relaxed);
                        if let Ok(p) = w.outer_position() {
                            save_popover_pos(p.x, p.y);
                        }
                        let _ = w.hide();
                    }
                    WindowEvent::Focused(false) => {
                        // A hidden window's "blur" (e.g. the one Windows fires at
                        // startup) carries a meaningless default position — only a
                        // VISIBLE popover the user clicks away from should be saved
                        // and hidden. Without this, startup persisted the OS's
                        // default placement and every open snapped there.
                        if !w.is_visible().unwrap_or(false) {
                            return;
                        }
                        // A title-bar drag momentarily blurs the window (the OS
                        // move loop); don't treat that as a click-away dismiss.
                        let dragging = w
                            .try_state::<DragGuard>()
                            .map(|g| now_ms() - g.0.load(Ordering::Relaxed) < 700)
                            .unwrap_or(false);
                        if dragging {
                            return;
                        }
                        lh.store(now_ms(), Ordering::Relaxed);
                        // Remember where the user left it (dragged or default) so
                        // the next open reuses this spot.
                        if let Ok(p) = w.outer_position() {
                            save_popover_pos(p.x, p.y);
                        }
                        let _ = w.hide();
                    }
                    _ => {}
                });
            }

            // Build the menu-bar tray: app glyph (template icon) + today's tokens.
            let dash = parser::build_dashboard();
            let label = tray_label(&dash, weekly_quota_display, language);
            let copy = tray_copy(language);
            let now_s = now_ms() / 1000;
            let (claude_limit, codex_limit) = provider_rows(&dash.provider_limits);

            let open_i = MenuItem::with_id(app, "open", copy.open, true, None::<&str>)?;
            let refresh_i = MenuItem::with_id(app, "refresh", copy.refresh, true, None::<&str>)?;
            let check_updates_i = MenuItem::with_id(
                app,
                "check-updates",
                copy.check_updates,
                true,
                None::<&str>,
            )?;
            // Provider Limits: one read-only row per provider, refreshed with
            // every dashboard build. Clicking a row opens the dashboard.
            let provider_claude_i = MenuItem::with_id(
                app,
                "provider-claude",
                provider_menu_row(claude_limit, "Claude", copy.provider_unavailable, now_s),
                true,
                None::<&str>,
            )?;
            let provider_codex_i = MenuItem::with_id(
                app,
                "provider-codex",
                provider_menu_row(codex_limit, "Codex", copy.provider_unavailable, now_s),
                true,
                None::<&str>,
            )?;
            let provider_limits_menu = Submenu::with_items(
                app,
                copy.provider_limits,
                true,
                &[&provider_claude_i, &provider_codex_i],
            )?;
            // Menu Bar Display: how much quota detail the title shows.
            let display_off_i = CheckMenuItem::with_id(
                app,
                "display-off",
                copy.off,
                true,
                weekly_quota_display == MenuBarQuotaDisplay::Off,
                None::<&str>,
            )?;
            let display_compact_i = CheckMenuItem::with_id(
                app,
                "display-compact",
                copy.compact,
                true,
                weekly_quota_display == MenuBarQuotaDisplay::Compact,
                None::<&str>,
            )?;
            let display_detailed_i = CheckMenuItem::with_id(
                app,
                "display-detailed",
                copy.detailed,
                true,
                weekly_quota_display == MenuBarQuotaDisplay::Detailed,
                None::<&str>,
            )?;
            let display_menu = Submenu::with_items(
                app,
                copy.menu_bar_display,
                true,
                &[&display_off_i, &display_compact_i, &display_detailed_i],
            )?;
            let dashboard_shortcut_i = CheckMenuItem::with_id(
                app,
                "dashboard-shortcut",
                dashboard_shortcut_menu_label(&dashboard_shortcut_key, language),
                true,
                dashboard_shortcut_on,
                None::<&str>,
            )?;
            let change_dashboard_shortcut_i = MenuItem::with_id(
                app,
                "change-dashboard-shortcut",
                copy.change_dashboard_shortcut,
                true,
                None::<&str>,
            )?;
            // Launch-at-login toggle (a checkbox item). Seeded from the reconciled
            // preference; clicking it flips the OS registration and persists.
            let autostart_i = CheckMenuItem::with_id(
                app,
                "autostart",
                copy.autostart,
                true,
                autostart_on,
                None::<&str>,
            )?;
            let language_en_i = CheckMenuItem::with_id(
                app,
                "language-en",
                copy.english,
                true,
                language == AppLanguage::En,
                None::<&str>,
            )?;
            let language_zh_i = CheckMenuItem::with_id(
                app,
                "language-zh",
                copy.chinese,
                true,
                language == AppLanguage::Zh,
                None::<&str>,
            )?;
            let language_menu = Submenu::with_items(
                app,
                copy.language,
                true,
                &[&language_en_i, &language_zh_i],
            )?;
            let quit_i = MenuItem::with_id(app, "quit", copy.quit, true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &open_i,
                    &refresh_i,
                    &check_updates_i,
                    &PredefinedMenuItem::separator(app)?,
                    &provider_limits_menu,
                    &display_menu,
                    &dashboard_shortcut_i,
                    &change_dashboard_shortcut_i,
                    &autostart_i,
                    &language_menu,
                    &PredefinedMenuItem::separator(app)?,
                    &quit_i,
                ],
            )?;
            app.manage(TrayMenuState {
                open: open_i.clone(),
                refresh: refresh_i.clone(),
                check_updates: check_updates_i.clone(),
                provider_limits: provider_limits_menu.clone(),
                provider_claude: provider_claude_i.clone(),
                provider_codex: provider_codex_i.clone(),
                menu_bar_display: display_menu.clone(),
                display_off: display_off_i.clone(),
                display_compact: display_compact_i.clone(),
                display_detailed: display_detailed_i.clone(),
                dashboard_shortcut: dashboard_shortcut_i.clone(),
                change_dashboard_shortcut: change_dashboard_shortcut_i.clone(),
                autostart: autostart_i.clone(),
                language: language_menu.clone(),
                language_en: language_en_i.clone(),
                language_zh: language_zh_i.clone(),
                quit: quit_i.clone(),
            });

            let lh_tray = last_hidden.clone();
            let _tray = TrayIconBuilder::with_id("main")
                .icon(tauri::include_image!("icons/tray-icon.png"))
                .icon_as_template(false)
                .title(&label)
                .tooltip(format!("Tokenscope · {} {}", copy.today, label))
                .menu(&menu)
                .show_menu_on_left_click(false) // left = toggle panel, right = menu
                .on_tray_icon_event(move |tray, event| {
                    let app = tray.app_handle();
                    tauri_plugin_positioner::on_tray_event(app, &event);
                    // Cache the tray-icon rect (physical px) for panel positioning.
                    // macOS aligns the panel under the menu-bar icon; Windows/Linux
                    // uses it to pick the monitor and pins the popover to that
                    // monitor's top-right — see position_panel / position_popover_windows.
                    if let TrayIconEvent::Click { rect, .. } = &event {
                        if let Some(anchor) = app.try_state::<TrayAnchor>() {
                            let p = rect.position.to_physical::<f64>(1.0);
                            let s = rect.size.to_physical::<f64>(1.0);
                            *anchor.0.lock().unwrap() = Some((p.x, p.y, s.width, s.height));
                        }
                    }
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        // if it was just hidden by the blur from this same click, leave it closed
                        let just_hidden = now_ms() - lh_tray.load(Ordering::Relaxed) < 250;
                        #[cfg(target_os = "macos")]
                        {
                            let visible = app
                                .get_webview_panel("main")
                                .map(|p| p.is_visible())
                                .unwrap_or(false);
                            if visible {
                                if let Ok(p) = app.get_webview_panel("main") {
                                    p.order_out(None);
                                }
                            } else if !just_hidden {
                                show_popover(app);
                            }
                        }
                        #[cfg(not(target_os = "macos"))]
                        {
                            let visible = app
                                .get_webview_window("main")
                                .and_then(|w| w.is_visible().ok())
                                .unwrap_or(false);
                            if visible {
                                if let Some(w) = app.get_webview_window("main") {
                                    let _ = w.hide();
                                }
                            } else if !just_hidden {
                                show_popover(app);
                            }
                        }
                    }
                })
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "open" => show_popover(app),
                    "refresh" => {
                        refresh(app);
                        refresh_pricing_bg(app);
                    }
                    "check-updates" => {
                        show_popover(app);
                        let _ = app.emit("check-for-updates", ());
                    }
                    "provider-claude" | "provider-codex" => {
                        // Rows are read-only summaries; clicking opens the panel.
                        show_popover(app);
                    }
                    "display-off" | "display-compact" | "display-detailed" => {
                        let display = match event.id.as_ref() {
                            "display-compact" => MenuBarQuotaDisplay::Compact,
                            "display-detailed" => MenuBarQuotaDisplay::Detailed,
                            _ => MenuBarQuotaDisplay::Off,
                        };
                        if let Some(state) = app.try_state::<TrayPreferencesState>() {
                            let Some(preferences) = state.0.lock().ok().map(|mut prefs| {
                                prefs.weekly_quota_display = display;
                                prefs.clone()
                            }) else {
                                return;
                            };
                            let _ = display_off_i.set_checked(display == MenuBarQuotaDisplay::Off);
                            let _ = display_compact_i
                                .set_checked(display == MenuBarQuotaDisplay::Compact);
                            let _ = display_detailed_i
                                .set_checked(display == MenuBarQuotaDisplay::Detailed);
                            save_tray_preferences(&preferences);
                            refresh(app);
                        }
                    }
                    "dashboard-shortcut" => {
                        if let Some(state) = app.try_state::<TrayPreferencesState>() {
                            let (enabled, shortcut) = state
                                .0
                                .lock()
                                .map(|prefs| {
                                    (
                                        prefs.dashboard_shortcut,
                                        prefs.dashboard_shortcut_key.clone(),
                                    )
                                })
                                .unwrap_or((false, DASHBOARD_SHORTCUT.to_string()));
                            let changed = if enabled {
                                app.global_shortcut().unregister(shortcut.as_str()).is_ok()
                            } else {
                                app.global_shortcut().register(shortcut.as_str()).is_ok()
                            };
                            if changed {
                                if let Ok(mut prefs) = state.0.lock() {
                                    prefs.dashboard_shortcut = !enabled;
                                    let _ = dashboard_shortcut_i.set_checked(!enabled);
                                    save_tray_preferences(&prefs);
                                }
                            }
                        }
                    }
                    "change-dashboard-shortcut" => {
                        let shortcut = app
                            .try_state::<TrayPreferencesState>()
                            .and_then(|state| {
                                state
                                    .0
                                    .lock()
                                    .ok()
                                    .map(|prefs| prefs.dashboard_shortcut_key.clone())
                            })
                            .unwrap_or_else(|| DASHBOARD_SHORTCUT.to_string());
                        show_popover(app);
                        let _ = app.emit("configure-dashboard-shortcut", shortcut);
                    }
                    "autostart" => {
                        // Flip the OS registration, re-read the real state, mirror
                        // it into the checkbox, and persist the user's choice.
                        let mgr = app.autolaunch();
                        let enabled = mgr.is_enabled().unwrap_or(false);
                        let _ = if enabled { mgr.disable() } else { mgr.enable() };
                        let now_on = mgr.is_enabled().unwrap_or(!enabled);
                        let _ = autostart_i.set_checked(now_on);
                        save_autostart_pref(now_on);
                    }
                    "language-en" | "language-zh" => {
                        let language = if event.id.as_ref() == "language-zh" {
                            "zh"
                        } else {
                            "en"
                        };
                        let _ = set_app_language(app.clone(), language.to_string());
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // Load prices off the main thread (the fetch can block ~20s on a
            // cold/stale cache) and refresh once a day. build_dashboard reads the
            // memoized copy, so neither JSON parsing nor the network ever runs
            // while BUILD_LOCK is held.
            let handle = app.handle().clone();
            std::thread::spawn(move || loop {
                pricing::Pricing::reload_shared(false);
                // Rebuild immediately with the newly loaded prices. Otherwise
                // an open panel can keep showing the startup built-in snapshot
                // (and false "without pricing data" warnings) until the next
                // unrelated 30-second refresh.
                refresh(&handle);
                std::thread::sleep(Duration::from_secs(24 * 60 * 60));
            });

            // Provider quotas are network-backed and independent from session
            // ingest. Refresh them off the dashboard build lock; a failed request
            // keeps the last successful API cache/log snapshot fallback intact.
            let handle = app.handle().clone();
            std::thread::spawn(move || loop {
                if quota_api::reload() {
                    refresh(&handle);
                }
                std::thread::sleep(Duration::from_secs(5 * 60));
            });

            // Background refresh: keep the tray's token count current and push
            // live updates to an open popover. Cheap thanks to incremental ingest.
            let handle = app.handle().clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(30));
                refresh(&handle);
            });

            // Filesystem watcher: reflect a log write within ~1s instead of
            // waiting up to the 30s poll (PRD wants <=5s). One watcher covers
            // every source root (~/.claude/projects, ~/.codex/sessions, …); our
            // own cache lives elsewhere, so this never self-triggers. Debounced
            // so a burst of writes coalesces into one rebuild; the 30s poll
            // above stays as a fallback. (build_dashboard serializes on
            // BUILD_LOCK, so this and the poll can't race the cache.)
            {
                let roots = store::source_roots();
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    use notify::{RecursiveMode, Watcher};
                    let (tx, rx) = std::sync::mpsc::channel();
                    let mut watcher = match notify::recommended_watcher(
                        move |res: notify::Result<notify::Event>| {
                            if res.is_ok() {
                                let _ = tx.send(());
                            }
                        },
                    ) {
                        Ok(w) => w,
                        Err(_) => return,
                    };
                    let mut watching = false;
                    for (_agent, dir) in &roots {
                        // The CLI may not have created its dir yet on a fresh
                        // machine; create it so watch() registers instead of
                        // silently falling back to the 30s poll all session.
                        let _ = std::fs::create_dir_all(dir);
                        if watcher.watch(dir, RecursiveMode::Recursive).is_ok() {
                            watching = true;
                        }
                    }
                    if !watching {
                        return;
                    }
                    // Block for the first change, then drain the burst until quiet.
                    while rx.recv().is_ok() {
                        while rx.recv_timeout(Duration::from_millis(400)).is_ok() {}
                        refresh(&handle);
                    }
                });
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_summaries_use_left_percentages() {
        let limit = model::ProviderLimit {
            provider: "codex".to_string(),
            label: "Codex".to_string(),
            plan: "Pro 5x".to_string(),
            windows: vec![
                model::LimitWindow {
                    id: "weekly".to_string(),
                    label: "Weekly".to_string(),
                    duration_minutes: 7 * 24 * 60,
                    used_pct: 71.0,
                    resets_at: 0,
                    as_of_ms: 0,
                    trend: Vec::new(),
                },
                model::LimitWindow {
                    id: "spark".to_string(),
                    label: "Spark".to_string(),
                    duration_minutes: 7 * 24 * 60,
                    used_pct: 69.0,
                    resets_at: 0,
                    as_of_ms: 0,
                    trend: Vec::new(),
                },
            ],
        };
        assert_eq!(provider_summary(&limit, false, 1), "Cx29%");
        assert_eq!(provider_summary(&limit, true, 1), "Cx W29%/S31%");
        assert_eq!(
            provider_menu_row(Some(&limit), "Codex", "Unavailable", 1),
            "Codex — W 29% · S 31%"
        );
    }

    #[test]
    fn provider_summary_hides_expired_windows() {
        let limit = model::ProviderLimit {
            provider: "claude".to_string(),
            label: "Claude".to_string(),
            plan: String::new(),
            windows: vec![
                model::LimitWindow {
                    id: "weekly".to_string(),
                    label: "Weekly".to_string(),
                    duration_minutes: 7 * 24 * 60,
                    used_pct: 20.0,
                    resets_at: 999,
                    as_of_ms: 0,
                    trend: Vec::new(),
                },
                model::LimitWindow {
                    id: "5h".to_string(),
                    label: "5-hour".to_string(),
                    duration_minutes: 300,
                    used_pct: 36.0,
                    resets_at: 2_000,
                    as_of_ms: 0,
                    trend: Vec::new(),
                },
            ],
        };
        // The expired weekly window drops out; only the 5h window remains.
        assert_eq!(provider_summary(&limit, true, 1_000), "Cl 5h64%");
        assert_eq!(
            provider_menu_row(Some(&limit), "Claude", "Unavailable", 1_000),
            "Claude — 5h 64%"
        );
        // Once every window is expired the provider disappears entirely.
        assert_eq!(provider_summary(&limit, false, 2_001), "");
    }

    #[test]
    fn old_tray_preferences_are_migrated() {
        let preferences = parse_tray_preferences(
            r#"{"show_weekly_remaining":true,"dashboard_shortcut":true}"#,
        );

        assert_eq!(preferences.weekly_quota_display, MenuBarQuotaDisplay::Compact);
        assert_eq!(preferences.dashboard_shortcut_key, DASHBOARD_SHORTCUT);
        assert_eq!(preferences.language, AppLanguage::En);
        assert!(DASHBOARD_SHORTCUT.parse::<Shortcut>().is_ok());
        assert!("Command+Alt+KeyT".parse::<Shortcut>().is_ok());
        assert_eq!(
            shortcut_label(DASHBOARD_SHORTCUT),
            if cfg!(target_os = "macos") {
                "⌥⌘T"
            } else {
                "Ctrl+Alt+T"
            }
        );
    }

    #[test]
    fn legacy_weekly_display_values_map_to_compact() {
        let off: MenuBarQuotaDisplay = serde_json::from_str(r#""off""#).unwrap();
        let compact: MenuBarQuotaDisplay = serde_json::from_str(r#""codex""#).unwrap();
        let compact_spark: MenuBarQuotaDisplay =
            serde_json::from_str(r#""codex_and_spark""#).unwrap();
        let detailed: MenuBarQuotaDisplay = serde_json::from_str(r#""detailed""#).unwrap();
        assert_eq!(off, MenuBarQuotaDisplay::Off);
        assert_eq!(compact, MenuBarQuotaDisplay::Compact);
        assert_eq!(compact_spark, MenuBarQuotaDisplay::Compact);
        assert_eq!(detailed, MenuBarQuotaDisplay::Detailed);
    }
}
