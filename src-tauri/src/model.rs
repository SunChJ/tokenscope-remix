// Shared data structures returned to the frontend.
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct SeriesPoint {
    pub label: String, // sparse axis label (many empty)
    pub full: String,  // complete label for the hover tooltip (hour / date)
    pub input: f64,    // M tokens (uncached new input)
    pub cache: f64,    // M tokens (cache creation + read)
    pub output: f64,   // M tokens
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelStat {
    pub name: String,
    pub vendor: String,
    pub tokens: f64, // M tokens (input + cache + output)
    #[serde(rename = "cacheTokens")]
    pub cache_tokens: f64, // M tokens (cache creation + read)
    pub cost: f64,   // USD estimate
    pub color: String,
    pub priced: bool, // false = no pricing data in LiteLLM (cost is unknown, not $0)
    // Owning agent id ("claude" / "codex"). In the All scope the same model name
    // can appear once per agent (e.g. gpt-5 via a router AND via Codex).
    pub agent: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NamedCount {
    pub name: String,
    pub count: u64,
}

/// Settlement summary for one local project/repository in the selected period.
/// The stable id is a hash of the local project root; raw paths never cross the
/// Tauri boundary.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectStat {
    pub id: String,
    pub name: String,
    pub tokens: f64,
    pub cost: f64,
    pub requests: u64,
    pub sessions: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ReliabilityStats {
    #[serde(rename = "completedTurns")]
    pub completed_turns: u64,
    #[serde(rename = "abortedTurns")]
    pub aborted_turns: u64,
    #[serde(rename = "toolErrors")]
    pub tool_errors: u64,
    pub denials: u64,
    #[serde(rename = "wastedTokens")]
    pub wasted_tokens: f64,
    #[serde(rename = "wastedCost")]
    pub wasted_cost: f64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct PerformanceStats {
    #[serde(rename = "trackedTurns")]
    pub tracked_turns: u64,
    #[serde(rename = "medianDurationMs")]
    pub median_duration_ms: u64,
    #[serde(rename = "p95DurationMs")]
    pub p95_duration_ms: u64,
    #[serde(rename = "medianTtftMs")]
    pub median_ttft_ms: u64,
    #[serde(rename = "p95TtftMs")]
    pub p95_ttft_ms: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ContextStats {
    #[serde(rename = "trackedTurns")]
    pub tracked_turns: u64,
    #[serde(rename = "medianPct")]
    pub median_pct: f64,
    #[serde(rename = "peakPct")]
    pub peak_pct: f64,
    #[serde(rename = "nearLimitTurns")]
    pub near_limit_turns: u64,
    pub compactions: u64,
    #[serde(rename = "reasoningTokens")]
    pub reasoning_tokens: f64,
    #[serde(rename = "reasoningPct")]
    pub reasoning_pct: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuotaTrendPoint {
    #[serde(rename = "tsMs")]
    pub ts_ms: i64,
    #[serde(rename = "usedPct")]
    pub used_pct: f64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Metrics {
    #[serde(rename = "totalTokens")]
    pub total_tokens: f64,
    #[serde(rename = "inputTokens")]
    pub input_tokens: f64,
    #[serde(rename = "cacheTokens")]
    pub cache_tokens: f64,
    #[serde(rename = "outputTokens")]
    pub output_tokens: f64,
    pub cost: f64,
    #[serde(rename = "mcpCalls")]
    pub mcp_calls: u64,
    #[serde(rename = "skillCalls")]
    pub skill_calls: u64,
    pub requests: u64,
    pub sessions: u64,
    #[serde(rename = "deltaTokens")]
    pub delta_tokens: f64,
    #[serde(rename = "deltaCost")]
    pub delta_cost: f64,
    pub servers: u64,
    pub skills: u64,
}

/// Per-agent share of a period, used only by the All scope: `tokens` drives the
/// hero split bar, `values` (aligned with `series`) drives the stacked chart.
#[derive(Debug, Clone, Serialize)]
pub struct AgentSlice {
    pub id: String,    // "claude" | "codex"
    pub label: String, // "Claude" | "Codex"
    pub color: String, // base hex, same across charts
    pub tokens: f64,   // M tokens in the period
    pub values: Vec<f64>, // M tokens per series bucket
}

#[derive(Debug, Clone, Serialize)]
pub struct PeriodReport {
    pub metrics: Metrics,
    pub series: Vec<SeriesPoint>,
    pub models: Vec<ModelStat>,
    pub projects: Vec<ProjectStat>,
    pub reliability: ReliabilityStats,
    pub performance: PerformanceStats,
    pub context: ContextStats,
    pub mcp: Vec<NamedCount>,
    pub skills: Vec<NamedCount>,
    #[serde(rename = "reqTrend")]
    pub req_trend: Vec<f64>,
    #[serde(rename = "costTrend")]
    pub cost_trend: Vec<f64>,
    // Non-empty only in the All scope when >=2 agents have data.
    pub agents: Vec<AgentSlice>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeatDay {
    pub date: String, // ISO yyyy-mm-dd
    pub tokens: f64,  // M tokens
    pub level: u8,    // 0..4
}

/// Codex rate-limit snapshot, straight from the newest token_count event's
/// rate_limits. `as_of_ms` lets the UI flag a stale reading.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Quota {
    pub plan: String,
    #[serde(rename = "primaryPct")]
    pub primary_pct: f64,
    #[serde(rename = "primaryMinutes")]
    pub primary_minutes: u64,
    #[serde(rename = "primaryResetsAt")]
    pub primary_resets_at: i64, // unix seconds
    #[serde(rename = "secondaryPct")]
    pub secondary_pct: f64,
    #[serde(rename = "secondaryMinutes")]
    pub secondary_minutes: u64,
    #[serde(rename = "secondaryResetsAt")]
    pub secondary_resets_at: i64, // unix seconds
    #[serde(rename = "asOfMs")]
    pub as_of_ms: i64,
}

/// One selectable view of the dashboard. With a single data source there is
/// exactly one scope (id "all", empty color → default theme, UI unchanged);
/// with several sources the first scope is the aggregate ("all") followed by
/// one scope per agent, and the UI shows filter chips.
#[derive(Debug, Clone, Serialize)]
pub struct Scope {
    pub id: String,    // "all" | "claude" | "codex"
    pub label: String, // "All" | "Claude" | "Codex"
    pub color: String, // agent accent hex; "" = default theme accent
    pub day: PeriodReport,
    pub week: PeriodReport,
    pub month: PeriodReport,
    pub heatmap: Vec<HeatDay>,
    pub quota: Option<Quota>, // Codex only
    #[serde(rename = "sparkQuota")]
    pub spark_quota: Option<Quota>, // codex_bengalfox only
    #[serde(rename = "quotaTrend")]
    pub quota_trend: Vec<QuotaTrendPoint>,
    #[serde(rename = "sparkQuotaTrend")]
    pub spark_quota_trend: Vec<QuotaTrendPoint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Dashboard {
    pub scopes: Vec<Scope>,
    #[serde(rename = "todayTokens")]
    pub today_tokens: f64, // M tokens across all agents, for the tray label
    #[serde(rename = "generatedAt")]
    pub generated_at: String,
    #[serde(rename = "telemetrySinceMs")]
    pub telemetry_since_ms: i64,
}

/// One scope's aggregation for an inclusive, user-selected date range.
#[derive(Debug, Clone, Serialize)]
pub struct RangeScope {
    pub id: String,
    pub report: PeriodReport,
}

#[derive(Debug, Clone, Serialize)]
pub struct RangeDashboard {
    pub scopes: Vec<RangeScope>,
    #[serde(rename = "startDate")]
    pub start_date: String,
    #[serde(rename = "endDate")]
    pub end_date: String,
}
