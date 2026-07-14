// Aggregate the store's normalized events (Claude + Codex) into per-scope
// Day / Week / Month reports + a daily heatmap. With one data source the
// dashboard has a single scope and looks exactly like the classic single-agent
// UI; with several, the first scope aggregates everything ("All") and is
// followed by one scope per agent, each with its own accent palette.
use crate::config::UserConfig;
use crate::model::*;
use crate::pricing::Pricing;
use crate::store::{RawEvent, Store, AGENT_CLAUDE, AGENT_CODEX};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, Timelike};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

// Serializes dashboard builds so the background refresh thread and the command
// handler never touch the incremental cache files concurrently.
static BUILD_LOCK: Mutex<()> = Mutex::new(());

// One API response, with config + pricing applied (derived per request from a
// RawEvent, since user config / prices / time windows can all change).
struct Event {
    ts: DateTime<Local>,
    session: String,
    model: String,
    input: f64,  // raw tokens, uncached new input only
    cache: f64,  // raw tokens, cache creation + read
    output: f64, // raw tokens
    cost: f64,   // USD (differentiated by token type), 0 if unknown model
    priced: bool, // whether a price was found for this model
    agent: &'static str, // owning agent id (interned via agent_def)
    project_id: String,
    project_name: String,
    mcp: Vec<String>,   // user-installed server names called in this msg
    skills: Vec<String>, // user-installed skill names called in this msg
}

#[derive(Clone)]
struct TurnEvent {
    ts: DateTime<Local>,
    agent: &'static str,
    outcome: String,
    input: f64,
    cache_creation: f64,
    cache_read: f64,
    output: f64,
    cost: f64,
    tool_errors: u64,
    denials: u64,
    duration_ms: u64,
    ttft_ms: u64,
    reasoning: f64,
    context_tokens: f64,
    context_window: f64,
    compactions: u64,
}

/// Static per-agent identity: label + accent + a 5-step chart palette (rank
/// shades of the accent, ending in a muted overflow tone).
pub struct AgentDef {
    pub id: &'static str,
    pub label: &'static str,
    pub color: &'static str,
    palette: [&'static str; 5],
}

/// Registry order = display order (chips, stacked charts, slices).
const AGENTS: &[AgentDef] = &[
    AgentDef {
        id: AGENT_CLAUDE,
        label: "Claude",
        color: "#d97757", // Anthropic coral
        palette: ["#c9683f", "#d97757", "#e59a78", "#f0c0a8", "#6d5147"],
    },
    AgentDef {
        id: AGENT_CODEX,
        label: "Codex",
        color: "#10a37f", // OpenAI teal
        palette: ["#0f8a6c", "#10a37f", "#4dbf9f", "#93dcc5", "#4a5f57"],
    },
];

fn agent_def(id: &str) -> &'static AgentDef {
    AGENTS.iter().find(|a| a.id == id).unwrap_or(&AGENTS[0])
}

// Single-source palette: the classic green/slate scheme (kept so a one-agent
// install looks exactly like before). Top-5 models get these; beyond is gray.
const PALETTE: &[&str] = &["#1f9d63", "#34c27e", "#6ad0a0", "#a7e3c5", "#4b5a52"];
const OVERFLOW_GRAY: &str = "#79817b";

/// Strip a trailing "-YYYYMMDD" date suffix so dated releases merge into
/// their base model (e.g. "claude-haiku-4-5-20251001" → "claude-haiku-4-5").
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn normalize_model(name: &str) -> String {
    if let Some(idx) = name.rfind('-') {
        let suffix = &name[idx + 1..];
        if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_digit()) {
            return name[..idx].to_string();
        }
    }
    name.to_string()
}

fn vendor_of(model: &str) -> &'static str {
    let m = model.to_lowercase();
    if m.contains("claude") {
        "Anthropic"
    } else if m.contains("gpt") || m.contains("o1") || m.contains("o3") || m.contains("codex") {
        "OpenAI"
    } else if m.contains("gemini") {
        "Google"
    } else if m.contains("llama") {
        "Local"
    } else if m.contains("glm") {
        "Zhipu"
    } else if m.contains("deepseek") {
        "DeepSeek"
    } else {
        "Other"
    }
}

fn quota_trend(
    history: &[crate::store::QuotaHistoryPoint],
    limit_id: &str,
    current: Option<&Quota>,
) -> Vec<QuotaTrendPoint> {
    let Some(current) = current else {
        return Vec::new();
    };
    let (used_pct, resets_at) = if current.primary_minutes == 7 * 24 * 60 {
        (current.primary_pct, current.primary_resets_at)
    } else if current.secondary_minutes == 7 * 24 * 60 {
        (current.secondary_pct, current.secondary_resets_at)
    } else {
        return Vec::new();
    };
    let mut points: Vec<QuotaTrendPoint> = history
        .iter()
        .filter(|point| point.limit_id == limit_id && point.resets_at == resets_at)
        .map(|point| QuotaTrendPoint {
            ts_ms: point.ts_ms,
            used_pct: point.used_pct,
        })
        .collect();
    points.sort_by_key(|point| point.ts_ms);
    if points.last().map(|point| point.ts_ms) != Some(current.as_of_ms) {
        points.push(QuotaTrendPoint {
            ts_ms: current.as_of_ms,
            used_pct,
        });
    }
    const MAX_POINTS: usize = 48;
    if points.len() <= MAX_POINTS {
        return points;
    }
    (0..MAX_POINTS)
        .map(|index| {
            let source = index * (points.len() - 1) / (MAX_POINTS - 1);
            points[source].clone()
        })
        .collect()
}

pub fn build_dashboard() -> Dashboard {
    let _guard = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // 1. Ingest incrementally (full scan only on first run; afterwards just the
    //    appended lines) and persist only when something actually changed. Keep
    //    complete history so user-selected billing ranges stay reproducible.
    let mut store = Store::load();
    if store.ingest() {
        store.save();
    }
    let codex_quota = store.codex_quota.clone();
    let codex_spark_quota = store.codex_spark_quota.clone();
    let codex_quota_trend = quota_trend(&store.quota_history, "codex", codex_quota.as_ref());
    let codex_spark_quota_trend = quota_trend(
        &store.quota_history,
        "codex_bengalfox",
        codex_spark_quota.as_ref(),
    );
    let telemetry_since_ms = store.telemetry_since_ms;

    // 2. Aggregate: apply current config + prices, slice by current time.
    let cfg = UserConfig::load(&store.codex_project_dirs());
    // Memoized price table (cheap clone); loaded/refreshed off-thread elsewhere
    // so neither parsing nor the network runs while we hold BUILD_LOCK.
    let pricing = Pricing::shared();
    let now = Local::now();
    // Preset reports and the heatmap need only recent events. Full history stays
    // in Store and is priced/aggregated only when a custom range requests it.
    let report_cutoff = (now - Duration::days(210)).timestamp_millis();
    let events: Vec<Event> = store
        .events
        .iter()
        .filter(|raw| raw.ts_ms >= report_cutoff)
        .map(|r| compute_event(r, store.project_for(r), &cfg, &pricing))
        .collect();
    let turns: Vec<TurnEvent> = store
        .turns
        .iter()
        .filter(|turn| turn.ts_ms >= report_cutoff)
        .map(|turn| compute_turn(turn, &pricing))
        .collect();

    let today = now.date_naive();

    // Keep historical agents selectable even when their latest event is outside
    // the preset window; custom ranges can still contain their usage.
    let present_ids: HashSet<&str> = store
        .events
        .iter()
        .map(|raw| agent_def(&raw.agent).id)
        .collect();
    let present: Vec<&AgentDef> = AGENTS.iter().filter(|a| present_ids.contains(a.id)).collect();

    let scopes = if present.len() <= 1 {
        // Single (or no) source → one scope, classic green UI, no chips.
        let agent = present.first().map(|a| a.id).unwrap_or(AGENT_CLAUDE);
        let mut scope = build_scope(
            "all", "All", "", &events, &turns, PALETTE, agent, &cfg, now,
        );
        if agent == AGENT_CODEX {
            scope.quota = codex_quota;
            scope.spark_quota = codex_spark_quota;
            scope.quota_trend = codex_quota_trend;
            scope.spark_quota_trend = codex_spark_quota_trend;
        }
        vec![scope]
    } else {
        // Aggregate scope first (default palette; models/slices merged below),
        // then one accent-colored scope per agent.
        let mut per_agent: Vec<Scope> = Vec::new();
        for a in &present {
            let filtered: Vec<Event> = events.iter().filter(|e| e.agent == a.id).map(clone_event).collect();
            let filtered_turns: Vec<TurnEvent> = turns
                .iter()
                .filter(|turn| turn.agent == a.id)
                .cloned()
                .collect();
            let mut s = build_scope(
                a.id,
                a.label,
                a.color,
                &filtered,
                &filtered_turns,
                &a.palette,
                a.id,
                &cfg,
                now,
            );
            if a.id == AGENT_CODEX {
                s.quota = codex_quota.clone();
                s.spark_quota = codex_spark_quota.clone();
                s.quota_trend = codex_quota_trend.clone();
                s.spark_quota_trend = codex_spark_quota_trend.clone();
            }
            per_agent.push(s);
        }
        let mut all = build_scope("all", "All", "", &events, &turns, PALETTE, "", &cfg, now);
        // All-scope model rows keep each agent's palette (rank within agent), and
        // each period gains per-agent slices for the split bar + stacked chart.
        for idx in 0..3usize {
            let mut models: Vec<ModelStat> = Vec::new();
            let mut slices: Vec<AgentSlice> = Vec::new();
            for (a, s) in present.iter().zip(per_agent.iter()) {
                let p = match idx {
                    0 => &s.day,
                    1 => &s.week,
                    _ => &s.month,
                };
                models.extend(p.models.iter().cloned());
                slices.push(AgentSlice {
                    id: a.id.to_string(),
                    label: a.label.to_string(),
                    color: a.color.to_string(),
                    tokens: p.metrics.total_tokens,
                    values: p.series.iter().map(|pt| pt.input + pt.cache + pt.output).collect(),
                });
            }
            models.sort_by(|x, y| y.tokens.partial_cmp(&x.tokens).unwrap_or(std::cmp::Ordering::Equal));
            let target = match idx {
                0 => &mut all.day,
                1 => &mut all.week,
                _ => &mut all.month,
            };
            target.models = models;
            target.agents = slices;
        }
        let mut v = vec![all];
        v.extend(per_agent);
        v
    };

    // today's displayed tokens (M) for the tray — across all agents
    let today_tokens: f64 = events
        .iter()
        .filter(|e| e.ts.date_naive() == today)
        .map(|e| (e.input + e.cache + e.output) / 1e6)
        .sum();

    Dashboard {
        scopes,
        today_tokens,
        generated_at: now.to_rfc3339(),
        telemetry_since_ms,
    }
}

/// Aggregate an inclusive custom date range for every dashboard scope. The
/// immediately preceding range of equal length is used for delta comparison.
pub fn build_range_dashboard(start: NaiveDate, end: NaiveDate) -> Result<RangeDashboard, String> {
    if start > end {
        return Err("start date must not be after end date".to_string());
    }

    let _guard = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = Store::load();
    if store.ingest() {
        store.save();
    }

    let cfg = UserConfig::load(&store.codex_project_dirs());
    let pricing = Pricing::shared();
    let days = (end - start).num_days() + 1;
    let previous_start = start
        .checked_sub_signed(Duration::days(days))
        .unwrap_or(start);
    let present_ids: HashSet<&str> = store
        .events
        .iter()
        .map(|raw| agent_def(&raw.agent).id)
        .collect();
    let events: Vec<Event> = store
        .events
        .iter()
        .filter(|raw| {
            let Some(timestamp) = DateTime::from_timestamp_millis(raw.ts_ms) else {
                return false;
            };
            let date = timestamp.with_timezone(&Local).date_naive();
            date >= previous_start && date <= end
        })
        .map(|raw| compute_event(raw, store.project_for(raw), &cfg, &pricing))
        .collect();
    let turns: Vec<TurnEvent> = store
        .turns
        .iter()
        .filter(|turn| {
            let Some(timestamp) = DateTime::from_timestamp_millis(turn.ts_ms) else {
                return false;
            };
            let date = timestamp.with_timezone(&Local).date_naive();
            date >= previous_start && date <= end
        })
        .map(|turn| compute_turn(turn, &pricing))
        .collect();
    let present: Vec<&AgentDef> = AGENTS
        .iter()
        .filter(|agent| present_ids.contains(agent.id))
        .collect();

    let scopes = if present.len() <= 1 {
        let agent = present.first().map(|a| a.id).unwrap_or(AGENT_CLAUDE);
        let mut report = report_range(&events, &turns, start, end, PALETTE);
        set_installed_counts(&mut report, agent, &cfg);
        vec![RangeScope {
            id: "all".to_string(),
            report,
        }]
    } else {
        let mut per_agent = Vec::new();
        for agent in &present {
            let filtered: Vec<Event> = events
                .iter()
                .filter(|event| event.agent == agent.id)
                .map(clone_event)
                .collect();
            let filtered_turns: Vec<TurnEvent> = turns
                .iter()
                .filter(|turn| turn.agent == agent.id)
                .cloned()
                .collect();
            let mut report = report_range(
                &filtered,
                &filtered_turns,
                start,
                end,
                &agent.palette,
            );
            set_installed_counts(&mut report, agent.id, &cfg);
            per_agent.push(RangeScope {
                id: agent.id.to_string(),
                report,
            });
        }

        let mut all = report_range(&events, &turns, start, end, PALETTE);
        set_installed_counts(&mut all, "", &cfg);
        all.models = per_agent
            .iter()
            .flat_map(|scope| scope.report.models.iter().cloned())
            .collect();
        all.models.sort_by(|a, b| {
            b.tokens
                .partial_cmp(&a.tokens)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        all.agents = present
            .iter()
            .zip(per_agent.iter())
            .map(|(agent, scope)| AgentSlice {
                id: agent.id.to_string(),
                label: agent.label.to_string(),
                color: agent.color.to_string(),
                tokens: scope.report.metrics.total_tokens,
                values: scope
                    .report
                    .series
                    .iter()
                    .map(|point| point.input + point.cache + point.output)
                    .collect(),
            })
            .collect();

        let mut scopes = vec![RangeScope {
            id: "all".to_string(),
            report: all,
        }];
        scopes.extend(per_agent);
        scopes
    };

    Ok(RangeDashboard {
        scopes,
        start_date: start.format("%Y-%m-%d").to_string(),
        end_date: end.format("%Y-%m-%d").to_string(),
    })
}

/// Cheap manual clone (Event holds Vec<String>s; used only for per-agent
/// filtering during multi-agent builds).
fn clone_event(e: &Event) -> Event {
    Event {
        ts: e.ts,
        session: e.session.clone(),
        model: e.model.clone(),
        input: e.input,
        cache: e.cache,
        output: e.output,
        cost: e.cost,
        priced: e.priced,
        agent: e.agent,
        project_id: e.project_id.clone(),
        project_name: e.project_name.clone(),
        mcp: e.mcp.clone(),
        skills: e.skills.clone(),
    }
}

/// Build one scope (all three period reports + heatmap) over `events`.
/// `agent_scope` is the agent id the scope represents ("" for the aggregate) —
/// it decides which installed-server/skill counts the metrics carry.
#[allow(clippy::too_many_arguments)]
fn build_scope(
    id: &str,
    label: &str,
    color: &str,
    events: &[Event],
    turns: &[TurnEvent],
    palette: &[&str],
    agent_scope: &str,
    cfg: &UserConfig,
    now: DateTime<Local>,
) -> Scope {
    let today = now.date_naive();
    let mut day = report_day(events, turns, now, palette);
    let mut week = report_week(events, turns, now, palette);
    let mut month = report_month(events, turns, now, palette);
    let heatmap = build_heatmap(events, today);

    for r in [&mut day, &mut week, &mut month] {
        set_installed_counts(r, agent_scope, cfg);
    }

    Scope {
        id: id.to_string(),
        label: label.to_string(),
        color: color.to_string(),
        day,
        week,
        month,
        heatmap,
        quota: None,
        spark_quota: None,
        quota_trend: Vec::new(),
        spark_quota_trend: Vec::new(),
    }
}

/// "servers"/"skills" are installed totals, not only the names called in the
/// selected period. This keeps preset and custom reports on the same footing.
fn set_installed_counts(report: &mut PeriodReport, agent_scope: &str, cfg: &UserConfig) {
    let (servers, skills) = match agent_scope {
        AGENT_CODEX => (cfg.codex_mcp_servers.len(), cfg.codex_skills.len()),
        AGENT_CLAUDE => (cfg.mcp_servers.len(), cfg.claude_skills.len()),
        _ => (
            cfg.mcp_servers.len() + cfg.codex_mcp_servers.len(),
            cfg.claude_skills.len() + cfg.codex_skills.len(),
        ),
    };
    report.metrics.servers = servers as u64;
    report.metrics.skills = skills as u64;
}

/// Derive a computed Event from a stored RawEvent, applying the *current* user
/// config (MCP/Skill whitelist) and prices. This is why these aren't baked into
/// the store: installing an MCP or a price refresh applies retroactively.
fn compute_event(
    r: &RawEvent,
    project: Option<&crate::store::ProjectRef>,
    cfg: &UserConfig,
    pricing: &Pricing,
) -> Event {
    let ts = DateTime::from_timestamp_millis(r.ts_ms)
        .unwrap_or_default()
        .with_timezone(&Local);
    let model = normalize_model(&r.model);
    // price lookup uses the raw (possibly dated) id, then the normalized one
    let cost_opt = pricing
        .cost(&r.model, r.in_tok, r.out_tok, r.cc, r.cr)
        .or_else(|| pricing.cost(&model, r.in_tok, r.out_tok, r.cc, r.cr));
    let mcp = r
        .mcp
        .iter()
        .filter(|s| cfg.is_user_mcp(&r.agent, s))
        .cloned()
        .collect();
    let skills = r
        .skills
        .iter()
        .filter(|s| cfg.is_user_skill(&r.agent, s))
        .map(|s| s.rsplit(':').next().unwrap_or(s).to_string())
        .collect();
    Event {
        ts,
        session: r.session.clone(),
        model,
        input: r.in_tok,
        cache: r.cc + r.cr,
        output: r.out_tok,
        cost: cost_opt.unwrap_or(0.0),
        priced: cost_opt.is_some(),
        agent: agent_def(&r.agent).id,
        project_id: project.map(|project| project.id.clone()).unwrap_or_default(),
        project_name: project
            .map(|project| project.name.clone())
            .unwrap_or_else(|| "Other".to_string()),
        mcp,
        skills,
    }
}

fn compute_turn(turn: &crate::store::TurnTelemetry, pricing: &Pricing) -> TurnEvent {
    let ts = DateTime::from_timestamp_millis(turn.ts_ms)
        .unwrap_or_default()
        .with_timezone(&Local);
    let cost = pricing
        .cost(
            &turn.model,
            turn.input_tokens,
            turn.output_tokens,
            turn.cache_creation_tokens,
            turn.cache_read_tokens,
        )
        .or_else(|| {
            pricing.cost(
                &normalize_model(&turn.model),
                turn.input_tokens,
                turn.output_tokens,
                turn.cache_creation_tokens,
                turn.cache_read_tokens,
            )
        })
        .unwrap_or(0.0);
    TurnEvent {
        ts,
        agent: agent_def(&turn.agent).id,
        outcome: turn.outcome.clone(),
        input: turn.input_tokens,
        cache_creation: turn.cache_creation_tokens,
        cache_read: turn.cache_read_tokens,
        output: turn.output_tokens,
        cost,
        tool_errors: turn.tool_errors,
        denials: turn.denials,
        duration_ms: turn.duration_ms,
        ttft_ms: turn.ttft_ms,
        reasoning: turn.reasoning_tokens,
        context_tokens: turn.context_tokens,
        context_window: turn.context_window,
        compactions: turn.compactions,
    }
}

fn reliability(turns: &[TurnEvent], start: NaiveDate, end: NaiveDate) -> ReliabilityStats {
    let mut stats = ReliabilityStats::default();
    for turn in turns {
        let date = turn.ts.date_naive();
        if date < start || date > end {
            continue;
        }
        match turn.outcome.as_str() {
            "completed" => stats.completed_turns += 1,
            "aborted" => {
                stats.aborted_turns += 1;
                stats.wasted_tokens +=
                    turn.input + turn.cache_creation + turn.cache_read + turn.output;
                stats.wasted_cost += turn.cost;
            }
            _ => {}
        }
        stats.tool_errors += turn.tool_errors;
        stats.denials += turn.denials;
    }
    stats.wasted_tokens = (stats.wasted_tokens / 1e6 * 1_000_000.0).round() / 1_000_000.0;
    stats.wasted_cost = (stats.wasted_cost * 1_000_000.0).round() / 1_000_000.0;
    stats
}

fn percentile(values: &mut [u64], percent: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let rank = (values.len() * percent).div_ceil(100).max(1);
    values[rank - 1]
}

fn performance(turns: &[TurnEvent], start: NaiveDate, end: NaiveDate) -> PerformanceStats {
    let mut durations: Vec<u64> = turns
        .iter()
        .filter(|turn| {
            let date = turn.ts.date_naive();
            date >= start && date <= end && turn.duration_ms > 0
        })
        .map(|turn| turn.duration_ms)
        .collect();
    let mut ttfts: Vec<u64> = turns
        .iter()
        .filter(|turn| {
            let date = turn.ts.date_naive();
            date >= start && date <= end && turn.ttft_ms > 0
        })
        .map(|turn| turn.ttft_ms)
        .collect();
    let tracked_turns = durations.len() as u64;
    let median_duration_ms = percentile(&mut durations, 50);
    let p95_duration_ms = percentile(&mut durations, 95);
    let median_ttft_ms = percentile(&mut ttfts, 50);
    let p95_ttft_ms = percentile(&mut ttfts, 95);
    PerformanceStats {
        tracked_turns,
        median_duration_ms,
        p95_duration_ms,
        median_ttft_ms,
        p95_ttft_ms,
    }
}

fn context_stats(turns: &[TurnEvent], start: NaiveDate, end: NaiveDate) -> ContextStats {
    let selected: Vec<&TurnEvent> = turns
        .iter()
        .filter(|turn| {
            let date = turn.ts.date_naive();
            date >= start && date <= end
        })
        .collect();
    let mut pressure: Vec<f64> = selected
        .iter()
        .filter(|turn| turn.context_window > 0.0 && turn.context_tokens > 0.0)
        .map(|turn| (turn.context_tokens / turn.context_window * 100.0).clamp(0.0, 100.0))
        .collect();
    pressure.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = if pressure.is_empty() {
        0.0
    } else {
        pressure[(pressure.len() - 1) / 2]
    };
    let peak = pressure.last().copied().unwrap_or(0.0);
    let reasoning: f64 = selected.iter().map(|turn| turn.reasoning).sum();
    let output: f64 = selected.iter().map(|turn| turn.output).sum();
    ContextStats {
        tracked_turns: pressure.len() as u64,
        median_pct: (median * 10.0).round() / 10.0,
        peak_pct: (peak * 10.0).round() / 10.0,
        near_limit_turns: pressure.iter().filter(|value| **value >= 80.0).count() as u64,
        compactions: selected.iter().map(|turn| turn.compactions).sum(),
        reasoning_tokens: (reasoning / 1e6 * 1_000_000.0).round() / 1_000_000.0,
        reasoning_pct: if output > 0.0 {
            (reasoning / output * 1000.0).round() / 10.0
        } else {
            0.0
        },
    }
}

// ── aggregation helpers ────────────────────────────────────────────
#[derive(Default)]
struct Agg {
    input: f64,
    cache: f64,
    output: f64,
    cost: f64,
    requests: u64,
    sessions: HashSet<String>,
    mcp_calls: u64,
    skill_calls: u64,
    model_tok: HashMap<String, f64>,
    model_cost: HashMap<String, f64>,
    model_priced: HashMap<String, bool>,
    model_agent: HashMap<String, &'static str>,
    projects: HashMap<String, ProjectAgg>,
    mcp_counts: HashMap<String, u64>,
    skill_counts: HashMap<String, u64>,
}

#[derive(Default)]
struct ProjectAgg {
    name: String,
    tokens: f64,
    cost: f64,
    requests: u64,
    sessions: HashSet<String>,
}

impl Agg {
    fn add(&mut self, e: &Event) {
        self.input += e.input;
        self.cache += e.cache;
        self.output += e.output;
        self.cost += e.cost;
        if !e.session.is_empty() {
            self.sessions.insert(e.session.clone());
        }
        // Slash-command skill / MCP-call-only events carry no model (empty) —
        // they're not LLM requests, so they must not inflate request counts or
        // the model split.
        if !e.model.is_empty() {
            self.requests += 1;
            // model totals keep all token types so shares sum to Total tokens
            *self.model_tok.entry(e.model.clone()).or_default() += e.input + e.cache + e.output;
            *self.model_cost.entry(e.model.clone()).or_default() += e.cost;
            // a model is "priced" if any of its messages had a known price
            *self.model_priced.entry(e.model.clone()).or_default() |= e.priced;
            self.model_agent.entry(e.model.clone()).or_insert(e.agent);
        }
        if !e.project_id.is_empty() {
            let project = self.projects.entry(e.project_id.clone()).or_default();
            project.name = e.project_name.clone();
            project.tokens += e.input + e.cache + e.output;
            project.cost += e.cost;
            if !e.model.is_empty() {
                project.requests += 1;
            }
            if !e.session.is_empty() {
                project.sessions.insert(e.session.clone());
            }
        }
        for s in &e.mcp {
            self.mcp_calls += 1;
            *self.mcp_counts.entry(s.clone()).or_default() += 1;
        }
        for s in &e.skills {
            self.skill_calls += 1;
            *self.skill_counts.entry(s.clone()).or_default() += 1;
        }
    }

    fn models(&self, palette: &[&str]) -> Vec<ModelStat> {
        let mut v: Vec<(String, f64, f64)> = self
            .model_tok
            .iter()
            .map(|(k, t)| (k.clone(), *t, *self.model_cost.get(k).unwrap_or(&0.0)))
            .collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        v.into_iter()
            .enumerate()
            .map(|(i, (name, tok, cost))| {
                let priced = *self.model_priced.get(&name).unwrap_or(&false);
                ModelStat {
                    vendor: vendor_of(&name).to_string(),
                    tokens: tok / 1e6,
                    cost: (cost * 1_000_000.0).round() / 1_000_000.0,
                    color: if i < palette.len() { palette[i] } else { OVERFLOW_GRAY }.to_string(),
                    priced,
                    agent: self.model_agent.get(&name).copied().unwrap_or("").to_string(),
                    name,
                }
            })
            .collect()
    }

    fn named(counts: &HashMap<String, u64>) -> Vec<NamedCount> {
        let mut v: Vec<NamedCount> = counts
            .iter()
            .map(|(k, c)| NamedCount {
                name: k.clone(),
                count: *c,
            })
            .collect();
        v.sort_by(|a, b| b.count.cmp(&a.count));
        v
    }

    fn projects(&self) -> Vec<ProjectStat> {
        let mut projects: Vec<ProjectStat> = self
            .projects
            .iter()
            .map(|(id, project)| ProjectStat {
                id: id.clone(),
                name: project.name.clone(),
                tokens: project.tokens / 1e6,
                cost: (project.cost * 1_000_000.0).round() / 1_000_000.0,
                requests: project.requests,
                sessions: project.sessions.len() as u64,
            })
            .collect();
        projects.sort_by(|a, b| {
            b.cost
                .partial_cmp(&a.cost)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    b.tokens
                        .partial_cmp(&a.tokens)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });
        projects
    }

    fn metrics(&self, delta_tokens: f64, delta_cost: f64) -> Metrics {
        Metrics {
            total_tokens: (self.input + self.cache + self.output) / 1e6,
            input_tokens: self.input / 1e6,
            cache_tokens: self.cache / 1e6,
            output_tokens: self.output / 1e6,
            cost: (self.cost * 1_000_000.0).round() / 1_000_000.0,
            mcp_calls: self.mcp_calls,
            skill_calls: self.skill_calls,
            requests: self.requests,
            sessions: self.sessions.len() as u64,
            delta_tokens,
            delta_cost,
            servers: self.mcp_counts.len() as u64,
            skills: self.skill_counts.len() as u64,
        }
    }
}

/// Percentage change of `cur` vs `prev`, e.g. +20.0 for a 20% increase,
/// rounded to 2 decimals. Returns 0 when there's no baseline to compare.
fn pct_delta(cur: f64, prev: f64) -> f64 {
    if prev <= 0.0 {
        return 0.0;
    }
    ((cur - prev) / prev * 10000.0).round() / 100.0
}

// ── Day report: today, 24 hourly buckets ───────────────────────────
fn report_day(
    events: &[Event],
    turns: &[TurnEvent],
    now: DateTime<Local>,
    palette: &[&str],
) -> PeriodReport {
    let today = now.date_naive();
    let yesterday = today - Duration::days(1);
    let mut agg = Agg::default();
    let mut prev = Agg::default();
    let mut buckets = vec![(0.0f64, 0.0f64, 0.0f64); 24]; // (input, cache, output) M
    let mut req_b = vec![0.0f64; 24];
    let mut cost_b = vec![0.0f64; 24];

    for e in events {
        let d = e.ts.date_naive();
        if d == today {
            agg.add(e);
            let h = e.ts.hour() as usize;
            buckets[h].0 += e.input / 1e6;
            buckets[h].1 += e.cache / 1e6;
            buckets[h].2 += e.output / 1e6;
            // Match Agg::add exactly: only the request COUNT excludes model-less
            // (slash-command) events; total cost accumulates unconditionally
            // (those events carry cost 0, so this is identical today).
            if !e.model.is_empty() {
                req_b[h] += 1.0;
            }
            cost_b[h] += e.cost;
        } else if d == yesterday {
            prev.add(e);
        }
    }

    let series = (0..24)
        .map(|h| SeriesPoint {
            // axis ticks every 4h, skipping the 00/24 endpoints
            label: if h % 4 == 0 && h != 0 {
                format!("{:02}", h)
            } else {
                String::new()
            },
            full: format!("{:02}:00", h),
            input: buckets[h].0,
            cache: buckets[h].1,
            output: buckets[h].2,
        })
        .collect();

    PeriodReport {
        metrics: agg.metrics(
            pct_delta(
                agg.input + agg.cache + agg.output,
                prev.input + prev.cache + prev.output,
            ),
            pct_delta(agg.cost, prev.cost),
        ),
        series,
        models: agg.models(palette),
        projects: agg.projects(),
        reliability: reliability(turns, today, today),
        performance: performance(turns, today, today),
        context: context_stats(turns, today, today),
        mcp: Agg::named(&agg.mcp_counts),
        skills: Agg::named(&agg.skill_counts),
        req_trend: req_b,
        cost_trend: cost_b,
        agents: Vec::new(),
    }
}

// ── Week report: current calendar week (Mon-Sun) vs previous week ────
fn report_week(
    events: &[Event],
    turns: &[TurnEvent],
    now: DateTime<Local>,
    palette: &[&str],
) -> PeriodReport {
    let today = now.date_naive();
    // Monday of the current week (Mon=0 … Sun=6).
    let start = today - Duration::days(today.weekday().num_days_from_monday() as i64);
    let next_start = start + Duration::days(7);
    let prev_start = start - Duration::days(7);

    let mut agg = Agg::default();
    let mut prev = Agg::default();
    let mut buckets = vec![(0.0f64, 0.0f64, 0.0f64); 7];
    let mut req_b = vec![0.0f64; 7];
    let mut cost_b = vec![0.0f64; 7];

    for e in events {
        let d = e.ts.date_naive();
        if d >= start && d < next_start {
            agg.add(e);
            let idx = (d - start).num_days() as usize;
            if idx < buckets.len() {
                buckets[idx].0 += e.input / 1e6;
                buckets[idx].1 += e.cache / 1e6;
                buckets[idx].2 += e.output / 1e6;
                // Match Agg::add: only the request COUNT excludes model-less
                // events; cost accumulates unconditionally (their cost is 0).
                if !e.model.is_empty() {
                    req_b[idx] += 1.0;
                }
                cost_b[idx] += e.cost;
            }
        } else if d >= prev_start && d < start {
            prev.add(e);
        }
    }

    let weekday = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let series = (0..7usize)
        .map(|i| {
            let date = start + Duration::days(i as i64);
            let wd = weekday[i];
            SeriesPoint {
                label: wd.to_string(),
                full: format!("{} {} {}", wd, MONTHS[(date.month() - 1) as usize], date.day()),
                input: buckets[i].0,
                cache: buckets[i].1,
                output: buckets[i].2,
            }
        })
        .collect();

    PeriodReport {
        metrics: agg.metrics(
            pct_delta(
                agg.input + agg.cache + agg.output,
                prev.input + prev.cache + prev.output,
            ),
            pct_delta(agg.cost, prev.cost),
        ),
        series,
        models: agg.models(palette),
        projects: agg.projects(),
        reliability: reliability(turns, start, next_start - Duration::days(1)),
        performance: performance(turns, start, next_start - Duration::days(1)),
        context: context_stats(turns, start, next_start - Duration::days(1)),
        mcp: Agg::named(&agg.mcp_counts),
        skills: Agg::named(&agg.skill_counts),
        req_trend: req_b,
        cost_trend: cost_b,
        agents: Vec::new(),
    }
}

// ── Month report: current calendar month vs previous calendar month ──
fn report_month(
    events: &[Event],
    turns: &[TurnEvent],
    now: DateTime<Local>,
    palette: &[&str],
) -> PeriodReport {
    let today = now.date_naive();
    let (y, m) = (today.year(), today.month());
    let cur_first = NaiveDate::from_ymd_opt(y, m, 1).unwrap();
    let next_first = if m == 12 {
        NaiveDate::from_ymd_opt(y + 1, 1, 1).unwrap()
    } else {
        NaiveDate::from_ymd_opt(y, m + 1, 1).unwrap()
    };
    let (py, pm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
    let prev_first = NaiveDate::from_ymd_opt(py, pm, 1).unwrap();
    let days_in_month = (next_first - cur_first).num_days() as usize;

    let mut agg = Agg::default();
    let mut prev = Agg::default();
    let mut buckets = vec![(0.0f64, 0.0f64, 0.0f64); days_in_month];
    let mut req_b = vec![0.0f64; days_in_month];
    let mut cost_b = vec![0.0f64; days_in_month];

    for e in events {
        let d = e.ts.date_naive();
        if d >= cur_first && d < next_first {
            agg.add(e);
            let idx = (d - cur_first).num_days() as usize;
            if idx < buckets.len() {
                buckets[idx].0 += e.input / 1e6;
                buckets[idx].1 += e.cache / 1e6;
                buckets[idx].2 += e.output / 1e6;
                // Match Agg::add: only the request COUNT excludes model-less
                // events; cost accumulates unconditionally (their cost is 0).
                if !e.model.is_empty() {
                    req_b[idx] += 1.0;
                }
                cost_b[idx] += e.cost;
            }
        } else if d >= prev_first && d < cur_first {
            prev.add(e);
        }
    }

    let series = (0..days_in_month)
        .map(|i| {
            let dn = (i + 1) as u32;
            let label = if i == 0 || dn % 5 == 0 {
                dn.to_string()
            } else {
                String::new()
            };
            SeriesPoint {
                label,
                full: format!("{} {}", MONTHS[(m - 1) as usize], dn),
                input: buckets[i].0,
                cache: buckets[i].1,
                output: buckets[i].2,
            }
        })
        .collect();

    PeriodReport {
        metrics: agg.metrics(
            pct_delta(
                agg.input + agg.cache + agg.output,
                prev.input + prev.cache + prev.output,
            ),
            pct_delta(agg.cost, prev.cost),
        ),
        series,
        models: agg.models(palette),
        projects: agg.projects(),
        reliability: reliability(turns, cur_first, next_first - Duration::days(1)),
        performance: performance(turns, cur_first, next_first - Duration::days(1)),
        context: context_stats(turns, cur_first, next_first - Duration::days(1)),
        mcp: Agg::named(&agg.mcp_counts),
        skills: Agg::named(&agg.skill_counts),
        req_trend: req_b,
        cost_trend: cost_b,
        agents: Vec::new(),
    }
}

// ── Custom report: inclusive dates vs the preceding equal-length range ────
fn report_range(
    events: &[Event],
    turns: &[TurnEvent],
    start: NaiveDate,
    end: NaiveDate,
    palette: &[&str],
) -> PeriodReport {
    let days = (end - start).num_days() + 1;
    let hourly = days == 1;
    // Preserve daily detail for ordinary billing windows. Longer ranges are
    // grouped into at most 48 contiguous buckets so charts stay cheap/readable;
    // totals and model/call breakdowns still use every event exactly.
    let bucket_days = if hourly { 1 } else { (days + 47) / 48 };
    let bucket_count = if hourly {
        24
    } else {
        ((days + bucket_days - 1) / bucket_days) as usize
    };
    let previous_start = start
        .checked_sub_signed(Duration::days(days))
        .unwrap_or(start);

    let mut agg = Agg::default();
    let mut previous = Agg::default();
    let mut buckets = vec![(0.0f64, 0.0f64, 0.0f64); bucket_count];
    let mut request_buckets = vec![0.0f64; bucket_count];
    let mut cost_buckets = vec![0.0f64; bucket_count];

    for event in events {
        let date = event.ts.date_naive();
        if date >= start && date <= end {
            agg.add(event);
            let index = if hourly {
                event.ts.hour() as usize
            } else {
                ((date - start).num_days() / bucket_days) as usize
            };
            if index < bucket_count {
                buckets[index].0 += event.input / 1e6;
                buckets[index].1 += event.cache / 1e6;
                buckets[index].2 += event.output / 1e6;
                if !event.model.is_empty() {
                    request_buckets[index] += 1.0;
                }
                cost_buckets[index] += event.cost;
            }
        } else if date >= previous_start && date < start {
            previous.add(event);
        }
    }

    let label_every = bucket_count.div_ceil(7).max(1);
    let series = (0..bucket_count)
        .map(|index| {
            let (label, full) = if hourly {
                (
                    if index % 4 == 0 && index != 0 {
                        format!("{:02}", index)
                    } else {
                        String::new()
                    },
                    format!("{} {:02}:00", full_date(start), index),
                )
            } else {
                let bucket_start = start + Duration::days(index as i64 * bucket_days);
                let bucket_end = std::cmp::min(
                    end,
                    bucket_start + Duration::days(bucket_days - 1),
                );
                let label = if index == 0
                    || index + 1 == bucket_count
                    || index % label_every == 0
                {
                    format!(
                        "{} {}",
                        MONTHS[(bucket_start.month() - 1) as usize],
                        bucket_start.day()
                    )
                } else {
                    String::new()
                };
                let full = if bucket_start == bucket_end {
                    full_date(bucket_start)
                } else {
                    format!("{} – {}", full_date(bucket_start), full_date(bucket_end))
                };
                (label, full)
            };
            SeriesPoint {
                label,
                full,
                input: buckets[index].0,
                cache: buckets[index].1,
                output: buckets[index].2,
            }
        })
        .collect();

    PeriodReport {
        metrics: agg.metrics(
            pct_delta(
                agg.input + agg.cache + agg.output,
                previous.input + previous.cache + previous.output,
            ),
            pct_delta(agg.cost, previous.cost),
        ),
        series,
        models: agg.models(palette),
        projects: agg.projects(),
        reliability: reliability(turns, start, end),
        performance: performance(turns, start, end),
        context: context_stats(turns, start, end),
        mcp: Agg::named(&agg.mcp_counts),
        skills: Agg::named(&agg.skill_counts),
        req_trend: request_buckets,
        cost_trend: cost_buckets,
        agents: Vec::new(),
    }
}

fn full_date(date: NaiveDate) -> String {
    format!(
        "{} {}, {}",
        MONTHS[(date.month() - 1) as usize],
        date.day(),
        date.year()
    )
}

// ── Heatmap: last ~26 weeks daily totals ────────────────────────────
fn build_heatmap(events: &[Event], today: chrono::NaiveDate) -> Vec<HeatDay> {
    let start = today - Duration::days(25 * 7 + today.weekday().num_days_from_sunday() as i64);
    let mut by_day: HashMap<chrono::NaiveDate, f64> = HashMap::new();
    for e in events {
        let d = e.ts.date_naive();
        if d >= start && d <= today {
            *by_day.entry(d).or_default() += (e.input + e.cache + e.output) / 1e6;
        }
    }
    let mut days = Vec::new();
    let mut d = start;
    let mut maxv = 0.0f64;
    while d <= today {
        let t = *by_day.get(&d).unwrap_or(&0.0);
        maxv = maxv.max(t);
        days.push((d, t));
        d += Duration::days(1);
    }
    days.into_iter()
        .map(|(date, tokens)| {
            let f = if maxv > 0.0 { tokens / maxv } else { 0.0 };
            let level = if tokens == 0.0 {
                0
            } else if f < 0.25 {
                1
            } else if f < 0.5 {
                2
            } else if f < 0.75 {
                3
            } else {
                4
            };
            HeatDay {
                date: date.format("%Y-%m-%d").to_string(),
                tokens: (tokens * 100.0).round() / 100.0,
                level,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn event(year: i32, month: u32, day: u32, hour: u32, tokens: f64) -> Event {
        Event {
            ts: Local
                .with_ymd_and_hms(year, month, day, hour, 0, 0)
                .single()
                .unwrap(),
            session: format!("{year}-{month}-{day}"),
            model: "test-model".to_string(),
            input: tokens,
            cache: 0.0,
            output: 0.0,
            cost: tokens / 1e6,
            priced: true,
            agent: AGENT_CLAUDE,
            project_id: "project-test".to_string(),
            project_name: "Test project".to_string(),
            mcp: Vec::new(),
            skills: Vec::new(),
        }
    }

    #[test]
    fn custom_range_is_inclusive_and_compares_equal_previous_range() {
        let events = vec![
            event(2026, 7, 1, 12, 1_000_000.0),
            event(2026, 7, 2, 12, 1_000_000.0),
            event(2026, 7, 3, 12, 2_000_000.0),
            event(2026, 7, 4, 12, 8_000_000.0),
        ];
        let report = report_range(
            &events,
            &[],
            NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
            NaiveDate::from_ymd_opt(2026, 7, 3).unwrap(),
            PALETTE,
        );

        assert_eq!(report.metrics.total_tokens, 3.0);
        assert_eq!(report.metrics.requests, 2);
        assert_eq!(report.metrics.delta_tokens, 200.0);
        assert_eq!(report.projects.len(), 1);
        assert_eq!(report.projects[0].name, "Test project");
        assert_eq!(report.projects[0].tokens, 3.0);
        assert_eq!(report.projects[0].requests, 2);
        assert_eq!(report.series.len(), 2);
        assert_eq!(report.series[0].input, 1.0);
        assert_eq!(report.series[1].input, 2.0);
    }

    #[test]
    fn custom_single_day_uses_hours_and_long_ranges_cap_chart_buckets() {
        let day = NaiveDate::from_ymd_opt(2026, 7, 13).unwrap();
        let hourly = report_range(&[event(2026, 7, 13, 23, 500.0)], &[], day, day, PALETTE);
        assert_eq!(hourly.series.len(), 24);
        assert_eq!(hourly.metrics.total_tokens, 0.0005);
        assert_eq!(hourly.series[23].input, 0.0005);

        let long = report_range(
            &[],
            &[],
            NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(2026, 4, 10).unwrap(),
            PALETTE,
        );
        assert!(long.series.len() <= 48);
    }

    #[test]
    fn reliability_counts_aborted_waste_and_tool_issues() {
        let ts = Local
            .with_ymd_and_hms(2026, 7, 13, 12, 0, 0)
            .single()
            .unwrap();
        let turns = vec![
            TurnEvent {
                ts,
                agent: AGENT_CODEX,
                outcome: "aborted".to_string(),
                input: 500_000.0,
                cache_creation: 0.0,
                cache_read: 400_000.0,
                output: 100_000.0,
                cost: 1.25,
                tool_errors: 2,
                denials: 1,
                duration_ms: 1_000,
                ttft_ms: 100,
                reasoning: 50_000.0,
                context_tokens: 160_000.0,
                context_window: 200_000.0,
                compactions: 1,
            },
            TurnEvent {
                ts,
                agent: AGENT_CODEX,
                outcome: "completed".to_string(),
                input: 0.0,
                cache_creation: 0.0,
                cache_read: 0.0,
                output: 0.0,
                cost: 0.0,
                tool_errors: 0,
                denials: 0,
                duration_ms: 3_000,
                ttft_ms: 300,
                reasoning: 0.0,
                context_tokens: 100_000.0,
                context_window: 200_000.0,
                compactions: 0,
            },
        ];
        let stats = reliability(&turns, ts.date_naive(), ts.date_naive());

        assert_eq!(stats.completed_turns, 1);
        assert_eq!(stats.aborted_turns, 1);
        assert_eq!(stats.tool_errors, 2);
        assert_eq!(stats.denials, 1);
        assert_eq!(stats.wasted_tokens, 1.0);
        assert_eq!(stats.wasted_cost, 1.25);

        let perf = performance(&turns, ts.date_naive(), ts.date_naive());
        assert_eq!(perf.tracked_turns, 2);
        assert_eq!(perf.median_duration_ms, 1_000);
        assert_eq!(perf.p95_duration_ms, 3_000);
        assert_eq!(perf.median_ttft_ms, 100);
        assert_eq!(perf.p95_ttft_ms, 300);

        let context = context_stats(&turns, ts.date_naive(), ts.date_naive());
        assert_eq!(context.tracked_turns, 2);
        assert_eq!(context.median_pct, 50.0);
        assert_eq!(context.peak_pct, 80.0);
        assert_eq!(context.near_limit_turns, 1);
        assert_eq!(context.compactions, 1);
        assert_eq!(context.reasoning_tokens, 0.05);
    }
}
