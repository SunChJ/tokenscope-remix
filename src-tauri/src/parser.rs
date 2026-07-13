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
    mcp: Vec<String>,   // user-installed server names called in this msg
    skills: Vec<String>, // user-installed skill names called in this msg
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
        .map(|r| compute_event(r, &cfg, &pricing))
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
        let mut scope = build_scope("all", "All", "", &events, PALETTE, agent, &cfg, now);
        if agent == AGENT_CODEX {
            scope.quota = codex_quota;
        }
        vec![scope]
    } else {
        // Aggregate scope first (default palette; models/slices merged below),
        // then one accent-colored scope per agent.
        let mut per_agent: Vec<Scope> = Vec::new();
        for a in &present {
            let filtered: Vec<Event> = events.iter().filter(|e| e.agent == a.id).map(clone_event).collect();
            let mut s = build_scope(a.id, a.label, a.color, &filtered, &a.palette, a.id, &cfg, now);
            if a.id == AGENT_CODEX {
                s.quota = codex_quota.clone();
            }
            per_agent.push(s);
        }
        let mut all = build_scope("all", "All", "", &events, PALETTE, "", &cfg, now);
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
        .map(|raw| compute_event(raw, &cfg, &pricing))
        .collect();
    let present: Vec<&AgentDef> = AGENTS
        .iter()
        .filter(|agent| present_ids.contains(agent.id))
        .collect();

    let scopes = if present.len() <= 1 {
        let agent = present.first().map(|a| a.id).unwrap_or(AGENT_CLAUDE);
        let mut report = report_range(&events, start, end, PALETTE);
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
            let mut report = report_range(&filtered, start, end, &agent.palette);
            set_installed_counts(&mut report, agent.id, &cfg);
            per_agent.push(RangeScope {
                id: agent.id.to_string(),
                report,
            });
        }

        let mut all = report_range(&events, start, end, PALETTE);
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
    palette: &[&str],
    agent_scope: &str,
    cfg: &UserConfig,
    now: DateTime<Local>,
) -> Scope {
    let today = now.date_naive();
    let mut day = report_day(events, now, palette);
    let mut week = report_week(events, now, palette);
    let mut month = report_month(events, now, palette);
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
fn compute_event(r: &RawEvent, cfg: &UserConfig, pricing: &Pricing) -> Event {
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
        mcp,
        skills,
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
    mcp_counts: HashMap<String, u64>,
    skill_counts: HashMap<String, u64>,
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
fn report_day(events: &[Event], now: DateTime<Local>, palette: &[&str]) -> PeriodReport {
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
        mcp: Agg::named(&agg.mcp_counts),
        skills: Agg::named(&agg.skill_counts),
        req_trend: req_b,
        cost_trend: cost_b,
        agents: Vec::new(),
    }
}

// ── Week report: current calendar week (Mon-Sun) vs previous week ────
fn report_week(events: &[Event], now: DateTime<Local>, palette: &[&str]) -> PeriodReport {
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
        mcp: Agg::named(&agg.mcp_counts),
        skills: Agg::named(&agg.skill_counts),
        req_trend: req_b,
        cost_trend: cost_b,
        agents: Vec::new(),
    }
}

// ── Month report: current calendar month vs previous calendar month ──
fn report_month(events: &[Event], now: DateTime<Local>, palette: &[&str]) -> PeriodReport {
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
            NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
            NaiveDate::from_ymd_opt(2026, 7, 3).unwrap(),
            PALETTE,
        );

        assert_eq!(report.metrics.total_tokens, 3.0);
        assert_eq!(report.metrics.requests, 2);
        assert_eq!(report.metrics.delta_tokens, 200.0);
        assert_eq!(report.series.len(), 2);
        assert_eq!(report.series[0].input, 1.0);
        assert_eq!(report.series[1].input, 2.0);
    }

    #[test]
    fn custom_single_day_uses_hours_and_long_ranges_cap_chart_buckets() {
        let day = NaiveDate::from_ymd_opt(2026, 7, 13).unwrap();
        let hourly = report_range(&[event(2026, 7, 13, 23, 500.0)], day, day, PALETTE);
        assert_eq!(hourly.series.len(), 24);
        assert_eq!(hourly.metrics.total_tokens, 0.0005);
        assert_eq!(hourly.series[23].input, 0.0005);

        let long = report_range(
            &[],
            NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(2026, 4, 10).unwrap(),
            PALETTE,
        );
        assert!(long.series.len() <= 48);
    }
}
