// Incremental event store.
//
// Ingestion (this file) is the only place that touches the JSONL logs. It
// parses each log line into a provider/config/price-independent RawEvent
// (just the facts), reads only newly-appended bytes of changed files (tracked
// by a per-file manifest), dedupes by message id, and persists everything to
// the cache dir. Aggregation (parser.rs) then works purely on these in-memory
// events — cheap, and recomputed per request because preset windows are
// relative to "now" and custom date ranges are selected at runtime.
//
// Two sources are ingested, normalized to the same RawEvent shape:
//   claude — ~/.claude/projects/**/*.jsonl   (assistant messages)
//   codex  — ~/.codex/sessions/**/*.jsonl    (token_count turn deltas)
use crate::model::Quota;
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub const AGENT_CLAUDE: &str = "claude";
pub const AGENT_CODEX: &str = "codex";

#[derive(Serialize, Deserialize, Clone)]
pub struct ProjectRef {
    pub id: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct TurnTelemetry {
    pub ts_ms: i64,
    pub source: String,
    pub agent: String,
    pub session: String,
    pub turn_id: String,
    pub model: String,
    pub outcome: String,
    pub abort_reason: String,
    pub input_tokens: f64,
    pub cache_creation_tokens: f64,
    pub cache_read_tokens: f64,
    pub output_tokens: f64,
    pub reasoning_tokens: f64,
    pub duration_ms: u64,
    pub ttft_ms: u64,
    pub tool_errors: u64,
    pub denials: u64,
    pub compactions: u64,
    pub context_tokens: f64,
    pub context_window: f64,
    // Claude can repeat one assistant message across several JSONL lines.
    // Persist ids so incremental restarts never count its usage twice.
    usage_ids: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct QuotaHistoryPoint {
    pub ts_ms: i64,
    pub limit_id: String,
    pub used_pct: f64,
    pub resets_at: i64,
}

#[derive(Serialize, Deserialize, Default)]
struct TelemetryCache {
    since_ms: i64,
    turns: Vec<TurnTelemetry>,
    #[serde(default)]
    quota_history: Vec<QuotaHistoryPoint>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RawEvent {
    pub ts_ms: i64,
    pub session: String,
    pub model: String, // raw model id (price lookup), normalized later for grouping
    pub in_tok: f64,   // uncached new input only (both agents)
    pub cc: f64, // cache creation (claude only; codex has no such concept → 0)
    pub cr: f64, // cache read (codex: cached_input_tokens, a subset of its raw input)
    pub out_tok: f64,
    pub mcp: Vec<String>,    // all mcp__<server> names called (unfiltered)
    pub skills: Vec<String>, // all detected Skill ids called (unfiltered)
    pub id: String,          // message id (dedup); "" = no cross-line dedup needed
    // Source log file (manifest key). Lets a truncated/rewritten file purge its
    // own stale events before being re-read, so re-ingestion stays idempotent.
    #[serde(default)]
    pub source: String,
    // Owning agent id (AGENT_*). Default keeps old caches readable, though a
    // version bump discards them anyway.
    #[serde(default = "default_agent")]
    pub agent: String,
}

fn default_agent() -> String {
    AGENT_CLAUDE.to_string()
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct FileState {
    size: u64,
    mtime_ms: i64,
    offset: u64, // bytes already ingested
    // Codex carry-over parse state: token_count lines don't repeat the model or
    // session, so an incremental read resuming mid-file needs the last-seen
    // values from the previous pass. Empty for claude files.
    #[serde(default)]
    model: String,
    #[serde(default)]
    session: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    turn_id: String,
    #[serde(default)]
    skills_seen: Vec<String>,
    // Newer Codex logs emit tool_search_output groups, then call the selected
    // tool by its short name (for example `js`). Persist the short-name mapping
    // so incremental reads can still attribute later calls to their MCP server.
    #[serde(default)]
    mcp_tools: HashMap<String, String>,
    // Codex forks a subagent/resumed thread by replaying the parent thread's
    // entire history — token_count, task_started/complete, tool calls, even the
    // parent's session_meta — into the head of the child's rollout file, all
    // restamped with the fork instant. Those lines are not new activity: the
    // parent file still holds them, and Codex events carry no message id to
    // dedupe on, so ingesting them double-counts tokens (measured ~5% of a
    // month) and duplicates turn telemetry. Skip everything until the child's
    // first `turn_context`, which is where its own turns begin.
    #[serde(default)]
    replaying: bool,
    // Whether this file's own `session_meta` (its first line) has been read.
    // Replayed parent metas follow it and must not overwrite session/cwd.
    #[serde(default)]
    meta_seen: bool,
    // Runtime-only source key used while appending telemetry records.
    #[serde(skip)]
    source: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Manifest {
    files: HashMap<String, FileState>,
}

pub struct Store {
    pub events: Vec<RawEvent>,
    // message id -> index in `events`. A single assistant message can be split
    // across several JSONL lines (e.g. thinking on one line, tool_use on the
    // next) that all share its id; we merge their tool calls into one event and
    // count its token usage only once.
    index: HashMap<String, usize>,
    manifest: Manifest,
    // Latest general and model-specific Codex rate-limit snapshots.
    pub codex_quota: Option<Quota>,
    pub codex_spark_quota: Option<Quota>,
    projects_by_source: HashMap<String, ProjectRef>,
    pub telemetry_since_ms: i64,
    pub turns: Vec<TurnTelemetry>,
    pub quota_history: Vec<QuotaHistoryPoint>,
    telemetry_index: HashMap<String, usize>,
}

// Bump when the parsing/extraction logic changes in a way that requires
// re-reading logs from scratch (the incremental manifest would otherwise skip
// already-seen bytes and miss newly-extracted facts).
//   v2: count slash-command skill invocations (`/skill`), not just Skill tool_use.
//   v3: merge tool_use across lines sharing a message id (a thinking line + a
//       tool_use line were deduped, dropping the tool call).
//   v4: track a per-event source file (idempotent re-read of truncated logs).
//   v5: multi-agent ingest (claude + codex), FileState manifest, quota snapshot.
//   v6: extract Codex Skill calls and track project skill directories.
//   v7: extract Codex MCP calls from tool search and app custom-tool formats.
//   v8: retain complete history for custom-range tracking and settlement.
//   v9: drop the replayed parent history at the head of a forked Codex thread
//       (it was double-counting tokens and turn telemetry).
const STORE_VERSION: u32 = 9;
const QUOTA_CACHE_VERSION: u32 = 1;
const PROJECT_CACHE_VERSION: u32 = 1;
const TELEMETRY_CACHE_VERSION: u32 = 1;
// One-time quota migration: at most 32 × 64 KiB = 2 MiB of log content.
const RECENT_QUOTA_FILES: usize = 32;
const QUOTA_TAIL_BYTES: u64 = 64 * 1024;

/// Atomically replace `path`'s contents: write a sibling temp file, then rename
/// over the target (same-volume rename is atomic on Windows and Unix). Avoids
/// the half-written/truncated JSON that a crash mid-`fs::write` would leave.
fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}

fn claude_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".claude").join("projects"))
}

/// ~/.codex/sessions, honoring the CODEX_HOME override the Codex CLI supports.
fn codex_dir() -> Option<PathBuf> {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| Some(dirs::home_dir()?.join(".codex")))?;
    Some(home.join("sessions"))
}

/// The log roots to ingest/watch: (agent id, directory). Missing directories
/// are simply skipped by the walker.
pub fn source_roots() -> Vec<(&'static str, PathBuf)> {
    let mut v = Vec::new();
    if let Some(d) = claude_dir() {
        v.push((AGENT_CLAUDE, d));
    }
    if let Some(d) = codex_dir() {
        v.push((AGENT_CODEX, d));
    }
    v
}

fn cache_dir() -> Option<PathBuf> {
    let d = dirs::cache_dir()?.join("tokenscope");
    let _ = fs::create_dir_all(&d);
    Some(d)
}

fn project_group(source: &str, agent: &str, state: Option<&FileState>) -> String {
    if agent == AGENT_CLAUDE {
        return Path::new(source)
            .parent()
            .unwrap_or_else(|| Path::new(source))
            .to_string_lossy()
            .to_string();
    }
    state
        .filter(|state| !state.cwd.is_empty())
        .map(|state| state.cwd.clone())
        .unwrap_or_else(|| source.to_string())
}

fn read_cwd_prefix(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(64 * 1024);
    file.take(64 * 1024).read_to_end(&mut bytes).ok()?;
    for line in bytes.split(|byte| *byte == b'\n') {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let cwd = value
            .get("cwd")
            .or_else(|| value.get("payload").and_then(|payload| payload.get("cwd")))
            .and_then(|cwd| cwd.as_str());
        if let Some(cwd) = cwd.filter(|cwd| !cwd.is_empty()) {
            return Some(cwd.to_string());
        }
    }
    None
}

fn project_root(cwd: &str) -> PathBuf {
    let path = PathBuf::from(cwd);
    for ancestor in path.ancestors() {
        if ancestor.join(".git").exists() {
            return ancestor.to_path_buf();
        }
    }
    path
}

fn stable_project_id(key: &str) -> String {
    // FNV-1a: tiny, deterministic across platforms and Rust releases.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in key.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("project-{hash:016x}")
}

fn telemetry_key(source: &str, turn_id: &str) -> String {
    format!("{source}\0{turn_id}")
}

fn project_ref(cwd: Option<&str>, fallback: &str) -> ProjectRef {
    let root = cwd.map(project_root).unwrap_or_else(|| PathBuf::from(fallback));
    let key = root.to_string_lossy();
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("Other")
        .to_string();
    ProjectRef {
        id: stable_project_id(&key),
        name,
    }
}

impl Store {
    /// Load persisted events + offset manifest (empty on first run).
    pub fn load() -> Self {
        let mut events: Vec<RawEvent> = Vec::new();
        let mut manifest = Manifest::default();
        let mut codex_quota = None;
        let mut codex_spark_quota = None;
        let mut projects_by_source = HashMap::new();
        let mut telemetry_since_ms = chrono::Utc::now().timestamp_millis();
        let mut turns = Vec::new();
        let mut quota_history = Vec::new();
        let mut version_ok = false;
        let mut quota_cache_ok = false;
        let mut telemetry_cache_ok = false;
        if let Some(dir) = cache_dir() {
            // If the cache was written by an older parser, discard it so ingest
            // does a full rescan and picks up newly-extracted facts.
            version_ok = fs::read_to_string(dir.join("version"))
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                == Some(STORE_VERSION);
            if version_ok {
                // events.json and offsets.json are ONE consistent unit: the
                // manifest's per-file byte offsets are only meaningful relative
                // to the events we actually loaded. If either is missing or fails
                // to parse (e.g. a crash left events.json half-written), discard
                // BOTH and fall back to a full rescan — otherwise a good manifest
                // paired with empty/corrupt events would make ingest() skip every
                // already-recorded file and silently lose all history.
                let loaded_events = fs::read_to_string(dir.join("events.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str::<Vec<RawEvent>>(&t).ok());
                let loaded_manifest = fs::read_to_string(dir.join("offsets.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str::<Manifest>(&t).ok());
                if let (Some(e), Some(m)) = (loaded_events, loaded_manifest) {
                    events = e;
                    manifest = m;
                }
                quota_cache_ok = fs::read_to_string(dir.join("quota_version"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    == Some(QUOTA_CACHE_VERSION);
                if quota_cache_ok {
                    codex_quota = fs::read_to_string(dir.join("codex_quota.json"))
                        .ok()
                        .and_then(|t| serde_json::from_str::<Quota>(&t).ok());
                    codex_spark_quota =
                        fs::read_to_string(dir.join("codex_bengalfox_quota.json"))
                            .ok()
                            .and_then(|t| serde_json::from_str::<Quota>(&t).ok());
                }
                let project_cache_ok = fs::read_to_string(dir.join("project_version"))
                    .ok()
                    .and_then(|text| text.trim().parse::<u32>().ok())
                    == Some(PROJECT_CACHE_VERSION);
                if project_cache_ok {
                    projects_by_source = fs::read_to_string(dir.join("projects.json"))
                        .ok()
                        .and_then(|text| serde_json::from_str(&text).ok())
                        .unwrap_or_default();
                }
                let telemetry_version_ok = fs::read_to_string(dir.join("telemetry_version"))
                    .ok()
                    .and_then(|text| text.trim().parse::<u32>().ok())
                    == Some(TELEMETRY_CACHE_VERSION);
                if telemetry_version_ok {
                    if let Some(cache) = fs::read_to_string(dir.join("telemetry.json"))
                        .ok()
                        .and_then(|text| serde_json::from_str::<TelemetryCache>(&text).ok())
                    {
                        telemetry_since_ms = cache.since_ms;
                        turns = cache.turns;
                        quota_history = cache.quota_history;
                        telemetry_cache_ok = true;
                    }
                }
            }
        }
        let index = events
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.id.is_empty())
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        let telemetry_index = turns
            .iter()
            .enumerate()
            .map(|(index, turn)| (telemetry_key(&turn.source, &turn.turn_id), index))
            .collect();
        let mut store = Store {
            events,
            index,
            manifest,
            codex_quota,
            codex_spark_quota,
            projects_by_source,
            telemetry_since_ms,
            turns,
            quota_history,
            telemetry_index,
        };
        if version_ok && !quota_cache_ok {
            store.rebuild_codex_quotas();
            if let Some(dir) = cache_dir() {
                store.save_quota_cache(&dir);
            }
        }
        if version_ok && store.refresh_projects() {
            if let Some(dir) = cache_dir() {
                store.save_project_cache(&dir);
            }
        }
        if version_ok && !telemetry_cache_ok {
            if let Some(dir) = cache_dir() {
                store.save_telemetry_cache(&dir);
            }
        }
        store
    }

    pub fn save(&self) {
        if let Some(dir) = cache_dir() {
            // Atomic writes so a crash/kill mid-save can't leave a half-written
            // events.json (load() would then discard the pair and lose history).
            // Write events before offsets: if we crash between them, the manifest
            // is merely stale (points at fewer bytes → re-reads a little) rather
            // than ahead of the events on disk.
            if let Ok(t) = serde_json::to_string(&self.events) {
                let _ = write_atomic(&dir.join("events.json"), t.as_bytes());
            }
            if let Ok(t) = serde_json::to_string(&self.manifest) {
                let _ = write_atomic(&dir.join("offsets.json"), t.as_bytes());
            }
            self.save_quota_cache(&dir);
            self.save_project_cache(&dir);
            self.save_telemetry_cache(&dir);
            let _ = write_atomic(&dir.join("version"), STORE_VERSION.to_string().as_bytes());
        }
    }

    fn save_telemetry_cache(&self, dir: &Path) {
        let cache = TelemetryCache {
            since_ms: self.telemetry_since_ms,
            turns: self.turns.clone(),
            quota_history: self.quota_history.clone(),
        };
        if let Ok(text) = serde_json::to_string(&cache) {
            let _ = write_atomic(&dir.join("telemetry.json"), text.as_bytes());
        }
        let _ = write_atomic(
            &dir.join("telemetry_version"),
            TELEMETRY_CACHE_VERSION.to_string().as_bytes(),
        );
    }

    fn save_project_cache(&self, dir: &Path) {
        if let Ok(text) = serde_json::to_string(&self.projects_by_source) {
            let _ = write_atomic(&dir.join("projects.json"), text.as_bytes());
        }
        let _ = write_atomic(
            &dir.join("project_version"),
            PROJECT_CACHE_VERSION.to_string().as_bytes(),
        );
    }

    fn save_quota_cache(&self, dir: &std::path::Path) {
        let save = |name: &str, quota: &Option<Quota>| match quota {
            Some(quota) => {
                if let Ok(text) = serde_json::to_string(quota) {
                    let _ = write_atomic(&dir.join(name), text.as_bytes());
                }
            }
            None => {
                let _ = fs::remove_file(dir.join(name));
            }
        };
        save("codex_quota.json", &self.codex_quota);
        save("codex_bengalfox_quota.json", &self.codex_spark_quota);
        let _ = write_atomic(
            &dir.join("quota_version"),
            QUOTA_CACHE_VERSION.to_string().as_bytes(),
        );
    }

    /// One-time migration from the old single-quota cache. Read only the tails
    /// of recently active Codex logs; token_count snapshots occur frequently,
    /// so this separates current quota buckets without rescanning usage history.
    fn rebuild_codex_quotas(&mut self) {
        self.codex_quota = None;
        self.codex_spark_quota = None;
        let Some(root) = codex_dir() else {
            return;
        };
        let mut files: Vec<_> = WalkDir::new(root)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .map(|x| x == "jsonl")
                    .unwrap_or(false)
            })
            .filter_map(|entry| {
                let modified = entry.metadata().ok()?.modified().ok()?;
                Some((modified, entry.into_path()))
            })
            .collect();
        files.sort_by_key(|item| std::cmp::Reverse(item.0));

        for (_, path) in files.into_iter().take(RECENT_QUOTA_FILES) {
            let Ok(mut file) = fs::File::open(path) else {
                continue;
            };
            let Ok(size) = file.metadata().map(|meta| meta.len()) else {
                continue;
            };
            let start = size.saturating_sub(QUOTA_TAIL_BYTES);
            if file.seek(SeekFrom::Start(start)).is_err() {
                continue;
            }
            let mut bytes = Vec::with_capacity((size - start) as usize);
            if file.read_to_end(&mut bytes).is_err() {
                continue;
            }
            let skip = if start == 0 {
                0
            } else {
                bytes
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map(|i| i + 1)
                    .unwrap_or(bytes.len())
            };
            let mut state = FileState::default();
            for line in bytes[skip..].split(|byte| *byte == b'\n') {
                let Ok(text) = std::str::from_utf8(line) else {
                    continue;
                };
                if text.contains("\"rate_limits\"") {
                    let _ = self.parse_codex_line(text, &mut state);
                }
            }
        }
    }

    /// Working directories seen in Codex sessions. Config uses these to find
    /// project-scoped `.agents/skills` without crawling the user's home dir.
    pub fn codex_project_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self
            .manifest
            .files
            .values()
            .filter(|s| !s.cwd.is_empty())
            .map(|s| PathBuf::from(&s.cwd))
            .collect();
        dirs.sort();
        dirs.dedup();
        dirs
    }

    pub fn project_for(&self, event: &RawEvent) -> Option<&ProjectRef> {
        self.projects_by_source.get(&event.source)
    }

    fn turn_mut(
        &mut self,
        state: &FileState,
        agent: &str,
        ts_ms: i64,
    ) -> Option<&mut TurnTelemetry> {
        if state.source.is_empty() || state.turn_id.is_empty() {
            return None;
        }
        if ts_ms > 0 && (self.telemetry_since_ms == 0 || ts_ms < self.telemetry_since_ms) {
            self.telemetry_since_ms = ts_ms;
        }
        let key = telemetry_key(&state.source, &state.turn_id);
        let index = if let Some(index) = self.telemetry_index.get(&key) {
            *index
        } else {
            let index = self.turns.len();
            self.turns.push(TurnTelemetry {
                ts_ms,
                source: state.source.clone(),
                agent: agent.to_string(),
                session: state.session.clone(),
                turn_id: state.turn_id.clone(),
                model: state.model.clone(),
                ..TurnTelemetry::default()
            });
            self.telemetry_index.insert(key, index);
            index
        };
        let turn = &mut self.turns[index];
        if turn.ts_ms == 0 || (ts_ms > 0 && ts_ms < turn.ts_ms) {
            turn.ts_ms = ts_ms;
        }
        if turn.session.is_empty() {
            turn.session = state.session.clone();
        }
        if !state.model.is_empty() {
            turn.model = state.model.clone();
        }
        Some(turn)
    }

    /// Resolve each source file to a stable project id and short display name.
    /// Existing Codex manifests already retain cwd. For old Claude caches, read
    /// at most one 64 KiB prefix per project directory, then persist the mapping
    /// so subsequent launches do no project-discovery IO.
    fn refresh_projects(&mut self) -> bool {
        let sources: HashMap<String, String> = self
            .events
            .iter()
            .filter(|event| !event.source.is_empty())
            .map(|event| (event.source.clone(), event.agent.clone()))
            .collect();
        let mut resolved_groups: HashMap<String, ProjectRef> = HashMap::new();

        for (source, agent) in &sources {
            if let Some(project) = self.projects_by_source.get(source) {
                resolved_groups
                    .entry(project_group(source, agent, self.manifest.files.get(source)))
                    .or_insert_with(|| project.clone());
            }
        }

        let mut dirty = false;
        for (source, agent) in sources {
            if self.projects_by_source.contains_key(&source) {
                continue;
            }
            let state = self.manifest.files.get(&source);
            let group = project_group(&source, &agent, state);
            let project = if let Some(project) = resolved_groups.get(&group) {
                project.clone()
            } else {
                let cwd = state
                    .filter(|state| !state.cwd.is_empty())
                    .map(|state| state.cwd.clone())
                    .or_else(|| read_cwd_prefix(Path::new(&source)));
                let project = project_ref(cwd.as_deref(), &group);
                resolved_groups.insert(group, project.clone());
                project
            };
            self.projects_by_source.insert(source, project);
            dirty = true;
        }
        dirty
    }

    /// Rebuild the id→index map after the `events` vector is mutated wholesale
    /// (purge/prune shift positions, so partial updates aren't enough).
    fn rebuild_index(&mut self) {
        self.index = self
            .events
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.id.is_empty())
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
    }

    fn rebuild_telemetry_index(&mut self) {
        self.telemetry_index = self
            .turns
            .iter()
            .enumerate()
            .map(|(index, turn)| (telemetry_key(&turn.source, &turn.turn_id), index))
            .collect();
    }

    /// Drop every event that came from `key`, then rebuild the index. Used before
    /// re-reading a truncated/rewritten file so re-ingestion is idempotent
    /// (otherwise the cross-line tool_use merge re-appends calls and id-less
    /// events get pushed twice, inflating MCP/Skill counts and token totals).
    fn purge_source(&mut self, key: &str) {
        self.events.retain(|e| e.source != key);
        self.rebuild_index();
        self.turns.retain(|turn| turn.source != key);
        self.rebuild_telemetry_index();
    }

    /// Incrementally read only the new bytes of new/changed JSONL files across
    /// all source roots. Returns whether anything changed (new events or an
    /// updated file offset), so the caller can skip a full cache rewrite when
    /// nothing moved.
    pub fn ingest(&mut self) -> bool {
        let mut dirty = false;
        for (agent, root) in source_roots() {
            if self.ingest_root(agent, &root) {
                dirty = true;
            }
        }
        if self.refresh_projects() {
            dirty = true;
        }
        dirty
    }

    fn ingest_root(&mut self, agent: &'static str, root: &PathBuf) -> bool {
        let mut dirty = false;
        for entry in WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
        {
            let path = entry.path();
            let key = path.to_string_lossy().to_string();
            let Ok(meta) = fs::metadata(path) else { continue };
            let size = meta.len();
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);

            let mut state = match self.manifest.files.get(&key).cloned() {
                Some(prev) => {
                    if prev.size == size && prev.mtime_ms == mtime_ms {
                        continue; // unchanged → skip
                    }
                    if size < prev.offset {
                        // truncated / rewritten (e.g. log compaction): the bytes
                        // we already ingested are gone, so purge this file's
                        // events and re-read from the start, idempotently.
                        self.purge_source(&key);
                        FileState::default()
                    } else {
                        prev
                    }
                }
                None => FileState::default(),
            };
            state.source = key.clone();

            let Ok(mut f) = fs::File::open(path) else { continue };
            if f.seek(SeekFrom::Start(state.offset)).is_err() {
                continue;
            }
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_err() {
                continue;
            }
            // only process up to the last newline; leave a partial trailing line
            // (file still being written) for the next pass
            let process_until = match buf.iter().rposition(|&b| b == b'\n') {
                Some(i) => i + 1,
                None => 0,
            };
            // Codex session ids also live in the filename (rollout-<ts>-<uuid>);
            // seed from it so events parsed before/without session_meta still
            // group into a session.
            if agent == AGENT_CODEX && state.session.is_empty() {
                state.session = codex_session_from_filename(path);
            }
            for line in buf[..process_until].split(|&b| b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let Ok(s) = std::str::from_utf8(line) else { continue };
                let parsed = match agent {
                    AGENT_CODEX => self.parse_codex_line(s, &mut state),
                    _ => self.parse_claude_line(s, &mut state),
                };
                if let Some(mut ev) = parsed {
                    ev.source = key.clone();
                    ev.agent = agent.to_string();
                    if !ev.id.is_empty() {
                        if let Some(&i) = self.index.get(&ev.id) {
                            // Same message, another line: merge its tool calls
                            // (don't re-count tokens — usage repeats per line).
                            let prev = &mut self.events[i];
                            prev.mcp.extend(ev.mcp);
                            prev.skills.extend(ev.skills);
                            continue;
                        }
                        self.index.insert(ev.id.clone(), self.events.len());
                    }
                    self.events.push(ev);
                }
            }
            state.offset += process_until as u64;
            state.size = size;
            state.mtime_ms = mtime_ms;
            self.manifest.files.insert(key, state);
            dirty = true;
        }
        dirty
    }

    /// Parse one Codex rollout line. Most lines are conversation items we skip;
    /// the ones that matter mutate the per-file parse state (model/session),
    /// update the quota snapshot, or produce a RawEvent (token_count deltas and
    /// MCP tool calls).
    fn parse_codex_line(&mut self, line: &str, state: &mut FileState) -> Option<RawEvent> {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let payload = v.get("payload")?;
        let line_type = v.get("type")?.as_str()?;
        // A forked thread replays its parent's history before its own first
        // turn_context. Drop that whole window (see FileState::replaying).
        if state.replaying && line_type != "turn_context" {
            return None;
        }
        match line_type {
            "session_meta" => {
                // Only the file's own meta (its first line) counts; the replayed
                // parent metas that follow would clobber session/cwd.
                if state.meta_seen {
                    return None;
                }
                state.meta_seen = true;
                // A fork/resume names the thread it continues; everything up to
                // this file's first turn_context is that thread's replayed log.
                state.replaying = payload.get("forked_from_id").is_some()
                    || payload.get("parent_thread_id").is_some();
                // `id` is this thread's own session id; `session_id` can be the
                // parent for subagent threads. Prefer the file's own id.
                if let Some(id) = payload
                    .get("id")
                    .and_then(|x| x.as_str())
                    .or_else(|| payload.get("session_id").and_then(|x| x.as_str()))
                {
                    state.session = id.to_string();
                }
                if let Some(cwd) = payload.get("cwd").and_then(|x| x.as_str()) {
                    state.cwd = cwd.to_string();
                }
                None
            }
            "turn_context" => {
                // The child's own turns start here; the replay window is over.
                state.replaying = false;
                if let Some(m) = payload.get("model").and_then(|x| x.as_str()) {
                    state.model = m.to_string();
                }
                if let Some(cwd) = payload.get("cwd").and_then(|x| x.as_str()) {
                    state.cwd = cwd.to_string();
                }
                if let Some(turn_id) = payload.get("turn_id").and_then(|x| x.as_str()) {
                    if state.turn_id != turn_id {
                        state.turn_id = turn_id.to_string();
                        state.skills_seen.clear();
                    }
                }
                None
            }
            "event_msg" => {
                let event_type = payload.get("type")?.as_str()?;
                let ts_ms = parse_ts(&v)?;
                if let Some(turn_id) = payload.get("turn_id").and_then(|value| value.as_str()) {
                    if state.turn_id != turn_id {
                        state.turn_id = turn_id.to_string();
                        state.skills_seen.clear();
                    }
                }
                match event_type {
                    "task_started" => {
                        state.skills_seen.clear();
                        let context_window = payload
                            .get("model_context_window")
                            .and_then(|value| value.as_f64())
                            .unwrap_or(0.0);
                        if let Some(turn) = self.turn_mut(state, AGENT_CODEX, ts_ms) {
                            turn.context_window = turn.context_window.max(context_window);
                        }
                        return None;
                    }
                    "task_complete" => {
                        if let Some(turn) = self.turn_mut(state, AGENT_CODEX, ts_ms) {
                            turn.outcome = "completed".to_string();
                            turn.duration_ms = payload
                                .get("duration_ms")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(0);
                            turn.ttft_ms = payload
                                .get("time_to_first_token_ms")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(0);
                        }
                        return None;
                    }
                    "turn_aborted" => {
                        if let Some(turn) = self.turn_mut(state, AGENT_CODEX, ts_ms) {
                            turn.outcome = "aborted".to_string();
                            turn.abort_reason = payload
                                .get("reason")
                                .and_then(|value| value.as_str())
                                .unwrap_or("")
                                .to_string();
                            turn.duration_ms = payload
                                .get("duration_ms")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(0);
                        }
                        return None;
                    }
                    "context_compacted" => {
                        if let Some(turn) = self.turn_mut(state, AGENT_CODEX, ts_ms) {
                            turn.compactions += 1;
                        }
                        return None;
                    }
                    "patch_apply_end" => {
                        if payload.get("success").and_then(|value| value.as_bool()) == Some(false) {
                            if let Some(turn) = self.turn_mut(state, AGENT_CODEX, ts_ms) {
                                turn.tool_errors += 1;
                            }
                        }
                        return None;
                    }
                    "token_count" => {}
                    _ => return None,
                }
                if let Some(rl) = payload.get("rate_limits") {
                    self.update_codex_quota(rl, ts_ms);
                }
                // Per-turn delta. `input_tokens` INCLUDES `cached_input_tokens`
                // (unlike Claude, where cache_read is separate) — subtract so
                // in_tok means "uncached new input" for both agents and
                // in + cr + out reproduces Codex's own total.
                let usage = payload.get("info")?.get("last_token_usage")?;
                let g = |k: &str| usage.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
                let raw_in = g("input_tokens");
                let cached = g("cached_input_tokens").min(raw_in);
                let out = g("output_tokens");
                let reasoning = g("reasoning_output_tokens").min(out);
                if raw_in + out <= 0.0 {
                    return None; // rate-limit-only heartbeat, nothing to count
                }
                let context_window = payload
                    .get("info")
                    .and_then(|info| info.get("model_context_window"))
                    .and_then(|value| value.as_f64())
                    .unwrap_or(0.0);
                if let Some(turn) = self.turn_mut(state, AGENT_CODEX, ts_ms) {
                    turn.input_tokens += raw_in - cached;
                    turn.cache_read_tokens += cached;
                    turn.output_tokens += out;
                    turn.reasoning_tokens += reasoning;
                    turn.context_tokens = turn.context_tokens.max(raw_in);
                    turn.context_window = turn.context_window.max(context_window);
                }
                Some(RawEvent {
                    ts_ms,
                    session: state.session.clone(),
                    model: state.model.clone(),
                    in_tok: raw_in - cached,
                    cc: 0.0,
                    cr: cached,
                    out_tok: out,
                    mcp: Vec::new(),
                    skills: Vec::new(),
                    id: String::new(), // no message id; file purge keeps re-reads idempotent
                    source: String::new(),
                    agent: String::new(),
                })
            }
            "response_item" => {
                let item_type = payload.get("type")?.as_str()?;
                let name = payload.get("name").and_then(|x| x.as_str()).unwrap_or("");

                // Newer Codex exposes lazily searched MCP tools under short
                // function names. Remember which server owns each returned tool.
                if item_type == "tool_search_output" {
                    remember_codex_mcp_tools(payload, state);
                    return None;
                }

                // Classic Codex CLI shell call. `arguments` is JSON containing
                // the command that reads a selected Skill's SKILL.md.
                if item_type == "function_call" && name == "exec_command" {
                    let args = payload.get("arguments")?.as_str()?;
                    let cmd = serde_json::from_str::<serde_json::Value>(args)
                        .ok()
                        .and_then(|a| a.get("cmd").and_then(|x| x.as_str()).map(str::to_owned))
                        .unwrap_or_else(|| args.to_string());
                    return codex_exec_event(&v, state, &cmd, false);
                }

                // Codex app wraps tool dispatch in a custom `exec` call. Its JS
                // input contains concrete SKILL.md paths and tools.mcp__ calls.
                if item_type == "custom_tool_call" && name == "exec" {
                    let input = payload.get("input")?.as_str()?;
                    return codex_exec_event(&v, state, input, true);
                }

                if item_type != "function_call" {
                    return None;
                }
                // Old logs keep the mcp__server__tool name. Newer tool-search
                // logs only keep the short tool name and need the mapping above.
                let server = name
                    .strip_prefix("mcp__")
                    .and_then(|rest| rest.split("__").next())
                    .filter(|server| !server.is_empty())
                    .map(str::to_owned)
                    .or_else(|| state.mcp_tools.get(name).cloned())?;
                let ts_ms = parse_ts(&v)?;
                Some(RawEvent {
                    ts_ms,
                    session: state.session.clone(),
                    model: String::new(), // not an LLM request → no tokens/cost
                    in_tok: 0.0,
                    cc: 0.0,
                    cr: 0.0,
                    out_tok: 0.0,
                    mcp: vec![server],
                    skills: Vec::new(),
                    id: String::new(),
                    source: String::new(),
                    agent: String::new(),
                })
            }
            _ => None,
        }
    }

    fn parse_claude_line(&mut self, line: &str, state: &mut FileState) -> Option<RawEvent> {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        let ts_ms = parse_ts(&value).unwrap_or(0);
        if let Some(session) = value.get("sessionId").and_then(|item| item.as_str()) {
            state.session = session.to_string();
        }
        if let Some(cwd) = value.get("cwd").and_then(|item| item.as_str()) {
            state.cwd = cwd.to_string();
        }

        match value.get("type").and_then(|item| item.as_str()) {
            Some("user") => {
                if value.get("interruptedMessageId").is_some() {
                    if let Some(turn) = self.turn_mut(state, AGENT_CLAUDE, ts_ms) {
                        turn.outcome = "aborted".to_string();
                        turn.abort_reason = "interrupted".to_string();
                    }
                }
                let is_tool_result = value.get("sourceToolUseID").is_some()
                    || value.get("sourceToolAssistantUUID").is_some();
                if !is_tool_result && value.get("isMeta").and_then(|item| item.as_bool()) != Some(true)
                {
                    if let Some(turn_id) = value
                        .get("promptId")
                        .or_else(|| value.get("uuid"))
                        .and_then(|item| item.as_str())
                    {
                        state.turn_id = turn_id.to_string();
                    }
                }
                let tool_errors = value
                    .get("message")
                    .and_then(|message| message.get("content"))
                    .and_then(|content| content.as_array())
                    .map(|content| {
                        content
                            .iter()
                            .filter(|block| {
                                block.get("type").and_then(|item| item.as_str())
                                    == Some("tool_result")
                                    && block.get("is_error").and_then(|item| item.as_bool())
                                        == Some(true)
                            })
                            .count() as u64
                    })
                    .unwrap_or(0);
                let denied = u64::from(value.get("toolDenialKind").is_some());
                if tool_errors > 0 || denied > 0 || !state.turn_id.is_empty() {
                    if let Some(turn) = self.turn_mut(state, AGENT_CLAUDE, ts_ms) {
                        turn.tool_errors += tool_errors;
                        turn.denials += denied;
                    }
                }
            }
            Some("assistant") => {
                let message = value.get("message").unwrap_or(&serde_json::Value::Null);
                if state.turn_id.is_empty() {
                    if let Some(turn_id) = value
                        .get("requestId")
                        .or_else(|| message.get("id"))
                        .and_then(|item| item.as_str())
                    {
                        state.turn_id = turn_id.to_string();
                    }
                }
                let usage_id = message
                    .get("id")
                    .and_then(|item| item.as_str())
                    .unwrap_or("");
                if let Some(turn) = self.turn_mut(state, AGENT_CLAUDE, ts_ms) {
                    if let Some(model) = message.get("model").and_then(|item| item.as_str()) {
                        turn.model = model.to_string();
                    }
                    if !usage_id.is_empty() && !turn.usage_ids.iter().any(|id| id == usage_id) {
                        let usage = message.get("usage").unwrap_or(&serde_json::Value::Null);
                        let number = |key: &str| {
                            usage
                                .get(key)
                                .and_then(|item| item.as_f64())
                                .unwrap_or(0.0)
                        };
                        turn.input_tokens += number("input_tokens");
                        turn.cache_creation_tokens += number("cache_creation_input_tokens");
                        turn.cache_read_tokens += number("cache_read_input_tokens");
                        turn.output_tokens += number("output_tokens");
                        turn.usage_ids.push(usage_id.to_string());
                    }
                }
            }
            Some("system") => match value.get("subtype").and_then(|item| item.as_str()) {
                Some("turn_duration") => {
                    if let Some(turn) = self.turn_mut(state, AGENT_CLAUDE, ts_ms) {
                        turn.outcome = "completed".to_string();
                        turn.duration_ms = value
                            .get("durationMs")
                            .and_then(|item| item.as_u64())
                            .unwrap_or(0);
                    }
                }
                Some("compact_boundary") => {
                    if let Some(turn) = self.turn_mut(state, AGENT_CLAUDE, ts_ms) {
                        turn.compactions += 1;
                    }
                }
                _ => {}
            },
            _ => {}
        }

        parse_claude_value(&value)
    }

    /// Keep the newest rate-limit snapshot (files are walked in arbitrary order,
    /// so compare timestamps rather than trusting encounter order).
    fn update_codex_quota(&mut self, rl: &serde_json::Value, ts_ms: i64) {
        let limit_id = match rl.get("limit_id").and_then(|value| value.as_str()) {
            None | Some("codex") => "codex",
            Some("codex_bengalfox") => "codex_bengalfox",
            _ => return,
        };
        let previous = if limit_id == "codex" {
            self.codex_quota.as_ref()
        } else {
            self.codex_spark_quota.as_ref()
        };
        if previous.map(|quota| quota.as_of_ms >= ts_ms).unwrap_or(false) {
            return;
        }
        let win = |k: &str| rl.get(k);
        let num = |w: Option<&serde_json::Value>, k: &str| {
            w.and_then(|x| x.get(k)).and_then(|x| x.as_f64()).unwrap_or(0.0)
        };
        let (p, s) = (win("primary"), win("secondary"));
        // A snapshot with no windows at all carries no information — skip it.
        if p.is_none() && s.is_none() {
            return;
        }
        let quota = Quota {
            plan: rl
                .get("plan_type")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            primary_pct: num(p, "used_percent"),
            primary_minutes: num(p, "window_minutes") as u64,
            primary_resets_at: num(p, "resets_at") as i64,
            secondary_pct: num(s, "used_percent"),
            secondary_minutes: num(s, "window_minutes") as u64,
            secondary_resets_at: num(s, "resets_at") as i64,
            as_of_ms: ts_ms,
        };
        self.record_quota_history(limit_id, &quota);
        if limit_id == "codex" {
            self.codex_quota = Some(quota);
        } else {
            self.codex_spark_quota = Some(quota);
        }
    }

    fn record_quota_history(&mut self, limit_id: &str, quota: &Quota) {
        let weekly = if quota.primary_minutes == 7 * 24 * 60 {
            Some((quota.primary_pct, quota.primary_resets_at))
        } else if quota.secondary_minutes == 7 * 24 * 60 {
            Some((quota.secondary_pct, quota.secondary_resets_at))
        } else {
            None
        };
        let Some((used_pct, resets_at)) = weekly else {
            return;
        };
        let last = self
            .quota_history
            .iter()
            .rev()
            .find(|point| point.limit_id == limit_id);
        if let Some(last) = last {
            if quota.as_of_ms <= last.ts_ms {
                return;
            }
            let same_value = (used_pct - last.used_pct).abs() < 0.1;
            let recent = quota.as_of_ms - last.ts_ms < 15 * 60 * 1000;
            if resets_at == last.resets_at && same_value && recent {
                return;
            }
        }
        self.quota_history.push(QuotaHistoryPoint {
            ts_ms: quota.as_of_ms,
            limit_id: limit_id.to_string(),
            used_pct,
            resets_at,
        });
        if self.quota_history.len().is_multiple_of(256) {
            let cutoff = quota.as_of_ms - 180 * 24 * 60 * 60 * 1000;
            self.quota_history.retain(|point| point.ts_ms >= cutoff);
        }
    }
}

/// RFC3339 top-level `timestamp` → epoch ms.
fn parse_ts(v: &serde_json::Value) -> Option<i64> {
    let ts = v.get("timestamp")?.as_str()?;
    Some(DateTime::parse_from_rfc3339(ts).ok()?.timestamp_millis())
}

/// Record short tool names returned by Codex's lazy tool search. A result group
/// named `mcp__node_repl__` with a child `js` means later `function_call: js`
/// events belong to the `node_repl` MCP server.
fn remember_codex_mcp_tools(payload: &serde_json::Value, state: &mut FileState) {
    let Some(groups) = payload.get("tools").and_then(|x| x.as_array()) else {
        return;
    };
    for group in groups {
        let Some(server) = group
            .get("name")
            .and_then(|x| x.as_str())
            .and_then(|name| name.strip_prefix("mcp__"))
            .and_then(|rest| rest.split("__").next())
            .filter(|server| !server.is_empty())
        else {
            continue;
        };
        let Some(tools) = group.get("tools").and_then(|x| x.as_array()) else {
            continue;
        };
        for tool in tools {
            if let Some(name) = tool.get("name").and_then(|x| x.as_str()) {
                state.mcp_tools.insert(name.to_string(), server.to_string());
            }
        }
    }
}

/// Extract every actual `tools.mcp__server__tool(...)` invocation from a Codex
/// app custom exec call. Repeated servers represent repeated tool calls.
fn codex_mcp_servers(text: &str) -> Vec<String> {
    const PREFIX: &str = "tools.mcp__";
    let mut servers = Vec::new();
    let mut offset = 0;
    while let Some(rel) = text[offset..].find(PREFIX) {
        let server_start = offset + rel + PREFIX.len();
        let rest = &text[server_start..];
        let Some(server_end) = rest.find("__") else {
            break;
        };
        let server = &rest[..server_end];
        let tool = &rest[server_end + 2..];
        let tool_end = tool
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(tool.len());
        if !server.is_empty() && tool_end > 0 && tool[tool_end..].trim_start().starts_with('(') {
            servers.push(server.to_string());
        }
        offset = server_start + server_end + 2 + tool_end;
    }
    servers
}

/// Extract each directory name immediately above a referenced SKILL.md. The
/// config whitelist later rejects built-in/plugin paths and unknown names.
fn codex_skill_names(text: &str) -> Vec<String> {
    let mut found: Vec<(usize, String)> = Vec::new();
    for marker in ["/SKILL.md", "\\SKILL.md"] {
        let mut offset = 0;
        while let Some(rel) = text[offset..].find(marker) {
            let end = offset + rel;
            let before = &text[..end];
            let path_start = before
                .rfind(|c: char| c.is_whitespace() || c == '\'' || c == '"')
                .map(|i| i + 1)
                .unwrap_or(0);
            let path = &before[path_start..];
            let built_in = path.contains("/skills/.system/")
                || path.contains("\\skills\\.system\\")
                || path.contains("/plugins/cache/")
                || path.contains("\\plugins\\cache\\");
            let start = path
                .rfind(['/', '\\'])
                .map(|i| i + 1)
                .unwrap_or(0);
            let name = path[start..]
                .trim_matches(|c: char| c.is_whitespace() || c == '\'' || c == '"');
            if !built_in && !name.is_empty() {
                found.push((end, name.to_string()));
            }
            offset = end + marker.len();
        }
    }
    found.sort_by_key(|(pos, _)| *pos);
    let mut names = Vec::new();
    for (_, name) in found {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// One call per skill per Codex turn. Reading the same SKILL.md again to finish
/// a truncated/paged read is still the same invocation and must not inflate it.
fn codex_exec_event(
    v: &serde_json::Value,
    state: &mut FileState,
    tool_input: &str,
    include_mcp: bool,
) -> Option<RawEvent> {
    let ts_ms = parse_ts(v)?;
    let skills: Vec<String> = codex_skill_names(tool_input)
        .into_iter()
        .filter(|skill| !state.skills_seen.contains(skill))
        .collect();
    let mcp = if include_mcp {
        codex_mcp_servers(tool_input)
    } else {
        Vec::new()
    };
    if skills.is_empty() && mcp.is_empty() {
        return None;
    }
    state.skills_seen.extend(skills.iter().cloned());
    Some(RawEvent {
        ts_ms,
        session: state.session.clone(),
        model: String::new(),
        in_tok: 0.0,
        cc: 0.0,
        cr: 0.0,
        out_tok: 0.0,
        mcp,
        skills,
        id: String::new(),
        source: String::new(),
        agent: String::new(),
    })
}

/// "rollout-2026-06-25T16-13-58-<uuid>.jsonl" → "<uuid>" (session id fallback).
fn codex_session_from_filename(path: &std::path::Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    // The uuid is everything after the timestamp: last 5 '-'-separated fields.
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() >= 5 {
        parts[parts.len() - 5..].join("-")
    } else {
        stem.to_string()
    }
}

/// Parse one Claude JSON value into a RawEvent (assistant messages only).
fn parse_claude_value(v: &serde_json::Value) -> Option<RawEvent> {
    match v.get("type")?.as_str()? {
        "assistant" => parse_assistant(v),
        // Skills invoked via slash command (e.g. `/find-skills`) are logged as a
        // user message with a <command-name> tag, NOT as a Skill tool_use, so
        // they need a separate path or they'd never be counted.
        "user" => parse_user_command(v),
        _ => None,
    }
}

/// Extract the inner text of `<tag>...</tag>` from `s`, if present.
fn extract_tag(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = s.find(&open)? + open.len();
    let rest = &s[start..];
    let end = rest.find(&close)?;
    Some(rest[..end].to_string())
}

/// A user message that is a slash-command invocation of a skill, e.g.
/// `<command-name>/find-skills</command-name>`. The skill name is left
/// unfiltered here; compute_event drops non-user skills via the whitelist.
fn parse_user_command(v: &serde_json::Value) -> Option<RawEvent> {
    let text = v.get("message")?.get("content")?.as_str()?;
    let raw = extract_tag(text, "command-name")?;
    let skill = raw.trim().trim_start_matches('/').trim().to_string();
    if skill.is_empty() {
        return None;
    }
    let ts_ms = parse_ts(v)?;
    let session = v
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    // dedup key: the line's own uuid (command messages have no message.id)
    let id = v.get("uuid").and_then(|i| i.as_str())?.to_string();
    if id.is_empty() {
        return None;
    }
    Some(RawEvent {
        ts_ms,
        session,
        model: String::new(), // not an LLM request → no model/tokens/cost
        in_tok: 0.0,
        cc: 0.0,
        cr: 0.0,
        out_tok: 0.0,
        mcp: Vec::new(),
        skills: vec![skill],
        id,
        source: String::new(),
        agent: String::new(),
    })
}

fn parse_assistant(v: &serde_json::Value) -> Option<RawEvent> {
    let msg = v.get("message")?;
    let model = msg.get("model").and_then(|m| m.as_str()).unwrap_or("unknown");
    if model == "<synthetic>" {
        return None;
    }
    let ts_ms = parse_ts(v)?;
    let session = v
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let id = msg
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or("")
        .to_string();

    let usage = msg.get("usage");
    let g = |k: &str| -> f64 {
        usage
            .and_then(|u| u.get(k))
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0)
    };

    let mut mcp = Vec::new();
    let mut skills = Vec::new();
    if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if let Some(rest) = name.strip_prefix("mcp__") {
                mcp.push(rest.split("__").next().unwrap_or("").to_string());
            } else if name == "Skill" {
                if let Some(sk) = block
                    .get("input")
                    .and_then(|i| i.get("skill"))
                    .and_then(|s| s.as_str())
                {
                    if !sk.is_empty() {
                        skills.push(sk.to_string());
                    }
                }
            }
        }
    }

    Some(RawEvent {
        ts_ms,
        session,
        model: model.to_string(),
        in_tok: g("input_tokens"),
        cc: g("cache_creation_input_tokens"),
        cr: g("cache_read_input_tokens"),
        out_tok: g("output_tokens"),
        mcp,
        skills,
        id,
        source: String::new(),
        agent: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_store() -> Store {
        Store {
            events: Vec::new(),
            index: HashMap::new(),
            manifest: Manifest::default(),
            codex_quota: None,
            codex_spark_quota: None,
            projects_by_source: HashMap::new(),
            telemetry_since_ms: 0,
            turns: Vec::new(),
            quota_history: Vec::new(),
            telemetry_index: HashMap::new(),
        }
    }

    #[test]
    fn keeps_general_and_spark_quotas_separate() {
        let mut store = empty_store();
        let snapshot = |limit_id: &str, used_percent: f64| {
            serde_json::json!({
                "limit_id": limit_id,
                "primary": {
                    "used_percent": used_percent,
                    "window_minutes": 10080,
                    "resets_at": 123
                }
            })
        };

        store.update_codex_quota(&snapshot("codex", 24.0), 100);
        store.update_codex_quota(&snapshot("codex_bengalfox", 7.0), 200);
        store.update_codex_quota(&snapshot("codex", 24.05), 300);

        assert_eq!(store.codex_quota.unwrap().primary_pct, 24.05);
        assert_eq!(store.codex_spark_quota.unwrap().primary_pct, 7.0);
        assert_eq!(store.quota_history.len(), 2);
    }

    // A forked/subagent thread replays the parent's whole history — usage, turn
    // telemetry, tool calls, even the parent's session_meta — at the head of its
    // rollout file, restamped with the fork instant. The parent file still holds
    // all of it and Codex events have no message id to dedupe on, so counting
    // the replay would double-count. Everything before the child's first
    // turn_context must be dropped.
    #[test]
    fn skips_replayed_parent_history_in_a_forked_codex_thread() {
        let mut store = empty_store();
        let mut state = FileState {
            source: "/tmp/child.jsonl".to_string(),
            ..FileState::default()
        };
        let own_meta = r#"{"timestamp":"2026-07-24T02:30:43Z","type":"session_meta","payload":{"id":"child-1","forked_from_id":"parent-1","parent_thread_id":"parent-1","cwd":"/work/child"}}"#;
        let replayed_meta = r#"{"timestamp":"2026-07-24T02:30:44Z","type":"session_meta","payload":{"id":"parent-1","cwd":"/work/parent"}}"#;
        let replayed_started = r#"{"timestamp":"2026-07-24T02:30:44Z","type":"event_msg","payload":{"type":"task_started","turn_id":"old-turn","model_context_window":200000}}"#;
        let replayed_usage = r#"{"timestamp":"2026-07-24T02:30:44Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":116643,"cached_input_tokens":114432,"output_tokens":592}}}}"#;
        let replayed_mcp = r#"{"timestamp":"2026-07-24T02:30:44Z","type":"response_item","payload":{"type":"function_call","name":"mcp__server__tool"}}"#;
        let own_context = r#"{"timestamp":"2026-07-24T02:31:00Z","type":"turn_context","payload":{"model":"gpt-5.6-sol","turn_id":"new-turn","cwd":"/work/child"}}"#;
        let own_usage = r#"{"timestamp":"2026-07-24T02:31:01Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":500,"cached_input_tokens":100,"output_tokens":50}}}}"#;

        assert!(store.parse_codex_line(own_meta, &mut state).is_none());
        assert!(store.parse_codex_line(replayed_meta, &mut state).is_none());
        assert!(store.parse_codex_line(replayed_started, &mut state).is_none());
        assert!(store.parse_codex_line(replayed_usage, &mut state).is_none());
        assert!(store.parse_codex_line(replayed_mcp, &mut state).is_none());
        // The replayed parent meta must not have hijacked the child's identity.
        assert_eq!(state.session, "child-1");
        assert_eq!(state.cwd, "/work/child");
        // Nothing before the first turn_context may produce turn telemetry.
        assert!(store.turns.is_empty());

        // The child's first turn_context ends the replay window; its own turns
        // count normally from there.
        assert!(store.parse_codex_line(own_context, &mut state).is_none());
        let event = store
            .parse_codex_line(own_usage, &mut state)
            .expect("the child's own usage still counts");
        assert_eq!(event.in_tok, 400.0);
        assert_eq!(event.cr, 100.0);
        assert_eq!(event.out_tok, 50.0);
        assert_eq!(event.model, "gpt-5.6-sol");
    }

    // A fresh (non-forked) session has no replay window, so its very first
    // events count even before any turn_context.
    #[test]
    fn keeps_all_events_in_a_fresh_codex_session() {
        let mut store = empty_store();
        let mut state = FileState::default();
        let meta = r#"{"timestamp":"2026-07-24T02:30:43Z","type":"session_meta","payload":{"id":"fresh-1","cwd":"/work"}}"#;
        let usage = r#"{"timestamp":"2026-07-24T02:30:44Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":300,"cached_input_tokens":100,"output_tokens":20}}}}"#;

        assert!(store.parse_codex_line(meta, &mut state).is_none());
        let event = store
            .parse_codex_line(usage, &mut state)
            .expect("a fresh session's usage counts from the start");
        assert_eq!(event.in_tok, 200.0);
        assert_eq!(event.session, "fresh-1");
    }

    #[test]
    fn records_codex_aborted_turn_and_wasted_tokens() {
        let mut store = empty_store();
        let mut state = FileState {
            source: "/tmp/codex.jsonl".to_string(),
            session: "session-1".to_string(),
            model: "gpt-5".to_string(),
            ..FileState::default()
        };
        let started = r#"{"timestamp":"2026-07-13T12:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1","model_context_window":200000}}"#;
        let usage = r#"{"timestamp":"2026-07-13T12:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"model_context_window":200000,"last_token_usage":{"input_tokens":1000,"cached_input_tokens":800,"output_tokens":100,"reasoning_output_tokens":40}}}}"#;
        let aborted = r#"{"timestamp":"2026-07-13T12:00:02Z","type":"event_msg","payload":{"type":"turn_aborted","turn_id":"turn-1","duration_ms":2000,"reason":"interrupted"}}"#;

        assert!(store.parse_codex_line(started, &mut state).is_none());
        assert!(store.parse_codex_line(usage, &mut state).is_some());
        assert!(store.parse_codex_line(aborted, &mut state).is_none());

        let turn = &store.turns[0];
        assert_eq!(turn.outcome, "aborted");
        assert_eq!(turn.input_tokens, 200.0);
        assert_eq!(turn.cache_read_tokens, 800.0);
        assert_eq!(turn.output_tokens, 100.0);
        assert_eq!(turn.reasoning_tokens, 40.0);
        assert_eq!(turn.duration_ms, 2000);
    }

    #[test]
    fn deduplicates_claude_turn_usage_and_counts_tool_errors() {
        let mut store = empty_store();
        let mut state = FileState {
            source: "/tmp/claude.jsonl".to_string(),
            ..FileState::default()
        };
        let user = r#"{"timestamp":"2026-07-13T12:00:00Z","type":"user","sessionId":"session-1","promptId":"prompt-1","message":{"content":"hi"}}"#;
        let assistant = r#"{"timestamp":"2026-07-13T12:00:01Z","type":"assistant","sessionId":"session-1","message":{"id":"message-1","model":"claude-sonnet-4","content":[],"usage":{"input_tokens":10,"cache_creation_input_tokens":20,"cache_read_input_tokens":30,"output_tokens":40}}}"#;
        let error = r#"{"timestamp":"2026-07-13T12:00:02Z","type":"user","sessionId":"session-1","sourceToolUseID":"tool-1","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","is_error":true,"content":"failed"}]}}"#;
        let duration = r#"{"timestamp":"2026-07-13T12:00:03Z","type":"system","sessionId":"session-1","subtype":"turn_duration","durationMs":3000}"#;

        let _ = store.parse_claude_line(user, &mut state);
        let _ = store.parse_claude_line(assistant, &mut state);
        let _ = store.parse_claude_line(assistant, &mut state);
        let _ = store.parse_claude_line(error, &mut state);
        let _ = store.parse_claude_line(duration, &mut state);

        let turn = &store.turns[0];
        assert_eq!(turn.outcome, "completed");
        assert_eq!(turn.input_tokens, 10.0);
        assert_eq!(turn.cache_creation_tokens, 20.0);
        assert_eq!(turn.cache_read_tokens, 30.0);
        assert_eq!(turn.output_tokens, 40.0);
        assert_eq!(turn.tool_errors, 1);
        assert_eq!(turn.duration_ms, 3000);
    }

    #[test]
    fn extracts_codex_skill_names_from_unix_and_windows_paths() {
        let input = r#"sed ~/.codex/skills/find-skills/SKILL.md && type C:\Users\me\.agents\skills\release-notes\SKILL.md && cat ~/.codex/skills/.system/openai-docs/SKILL.md && cat ~/.codex/plugins/cache/bundled/skills/browser/SKILL.md"#;
        assert_eq!(
            codex_skill_names(input),
            vec!["find-skills".to_string(), "release-notes".to_string()]
        );
    }

    #[test]
    fn counts_a_codex_skill_once_per_turn() {
        let mut store = empty_store();
        let mut state = FileState::default();
        let turn = |id: &str| {
            format!(
                r#"{{"timestamp":"2026-07-09T12:00:00Z","type":"turn_context","payload":{{"turn_id":"{id}","model":"gpt-5"}}}}"#
            )
        };
        let call = r#"{"timestamp":"2026-07-09T12:00:01Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"read ~/.codex/skills/find-skills/SKILL.md"}}"#;

        assert!(store.parse_codex_line(&turn("turn-1"), &mut state).is_none());
        assert_eq!(
            store.parse_codex_line(call, &mut state).unwrap().skills,
            vec!["find-skills"]
        );
        assert!(store.parse_codex_line(call, &mut state).is_none());

        assert!(store.parse_codex_line(&turn("turn-2"), &mut state).is_none());
        assert_eq!(
            store.parse_codex_line(call, &mut state).unwrap().skills,
            vec!["find-skills"]
        );
    }

    #[test]
    fn parses_classic_codex_exec_command_skill_call() {
        let mut store = empty_store();
        let mut state = FileState::default();
        let call = r#"{"timestamp":"2026-07-09T12:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cat ~/.agents/skills/review/SKILL.md\"}"}}"#;

        assert_eq!(
            store.parse_codex_line(call, &mut state).unwrap().skills,
            vec!["review"]
        );
    }

    #[test]
    fn maps_tool_search_results_to_short_mcp_calls() {
        let mut store = empty_store();
        let mut state = FileState::default();
        let search = r#"{"timestamp":"2026-07-09T12:00:00Z","type":"response_item","payload":{"type":"tool_search_output","tools":[{"name":"mcp__node_repl__","tools":[{"name":"js"},{"name":"js_reset"}]}]}}"#;
        let call = r#"{"timestamp":"2026-07-09T12:00:01Z","type":"response_item","payload":{"type":"function_call","name":"js","arguments":"{}"}}"#;

        assert!(store.parse_codex_line(search, &mut state).is_none());
        assert_eq!(
            store.parse_codex_line(call, &mut state).unwrap().mcp,
            vec!["node_repl"]
        );
    }

    #[test]
    fn extracts_mcp_calls_from_codex_app_custom_exec() {
        let mut store = empty_store();
        let mut state = FileState::default();
        let call = r#"{"timestamp":"2026-07-09T12:00:01Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"await tools.mcp__node_repl__js({}); await tools.mcp__node_repl__js({}); const ref = tools.mcp__unused__inspect;"}}"#;

        assert_eq!(
            store.parse_codex_line(call, &mut state).unwrap().mcp,
            vec!["node_repl", "node_repl"]
        );
    }

    #[test]
    fn parses_prefixed_codex_mcp_call() {
        let mut store = empty_store();
        let mut state = FileState::default();
        let call = r#"{"timestamp":"2026-07-09T12:00:01Z","type":"response_item","payload":{"type":"function_call","name":"mcp__chrome_devtools__click","arguments":"{}"}}"#;

        assert_eq!(
            store.parse_codex_line(call, &mut state).unwrap().mcp,
            vec!["chrome_devtools"]
        );
    }
}
