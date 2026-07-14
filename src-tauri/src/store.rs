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
use std::path::PathBuf;
use walkdir::WalkDir;

pub const AGENT_CLAUDE: &str = "claude";
pub const AGENT_CODEX: &str = "codex";

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
const STORE_VERSION: u32 = 8;
const QUOTA_CACHE_VERSION: u32 = 1;
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

impl Store {
    /// Load persisted events + offset manifest (empty on first run).
    pub fn load() -> Self {
        let mut events: Vec<RawEvent> = Vec::new();
        let mut manifest = Manifest::default();
        let mut codex_quota = None;
        let mut codex_spark_quota = None;
        let mut version_ok = false;
        let mut quota_cache_ok = false;
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
            }
        }
        let index = events
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.id.is_empty())
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        let mut store = Store {
            events,
            index,
            manifest,
            codex_quota,
            codex_spark_quota,
        };
        if version_ok && !quota_cache_ok {
            store.rebuild_codex_quotas();
            if let Some(dir) = cache_dir() {
                store.save_quota_cache(&dir);
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
            let _ = write_atomic(&dir.join("version"), STORE_VERSION.to_string().as_bytes());
        }
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
        files.sort_by(|a, b| b.0.cmp(&a.0));

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

    /// Drop every event that came from `key`, then rebuild the index. Used before
    /// re-reading a truncated/rewritten file so re-ingestion is idempotent
    /// (otherwise the cross-line tool_use merge re-appends calls and id-less
    /// events get pushed twice, inflating MCP/Skill counts and token totals).
    fn purge_source(&mut self, key: &str) {
        self.events.retain(|e| e.source != key);
        self.rebuild_index();
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
                    _ => parse_claude_line(s),
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
        match v.get("type")?.as_str()? {
            "session_meta" => {
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
                // Older Codex logs may not carry turn_context.turn_id. A task
                // boundary is still enough to reset per-turn Skill dedup.
                if event_type == "task_started" {
                    state.skills_seen.clear();
                    return None;
                }
                if event_type != "token_count" {
                    return None;
                }
                let ts_ms = parse_ts(&v)?;
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
                if raw_in + out <= 0.0 {
                    return None; // rate-limit-only heartbeat, nothing to count
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

    /// Keep the newest rate-limit snapshot (files are walked in arbitrary order,
    /// so compare timestamps rather than trusting encounter order).
    fn update_codex_quota(&mut self, rl: &serde_json::Value, ts_ms: i64) {
        let slot = match rl.get("limit_id").and_then(|value| value.as_str()) {
            None | Some("codex") => &mut self.codex_quota,
            Some("codex_bengalfox") => &mut self.codex_spark_quota,
            _ => return,
        };
        if slot.as_ref().map(|q| q.as_of_ms >= ts_ms).unwrap_or(false) {
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
        *slot = Some(Quota {
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
        });
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
                .rfind(|c| c == '/' || c == '\\')
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

/// Parse one Claude JSONL line into a RawEvent (assistant messages only).
fn parse_claude_line(line: &str) -> Option<RawEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    match v.get("type")?.as_str()? {
        "assistant" => parse_assistant(&v),
        // Skills invoked via slash command (e.g. `/find-skills`) are logged as a
        // user message with a <command-name> tag, NOT as a Skill tool_use, so
        // they need a separate path or they'd never be counted.
        "user" => parse_user_command(&v),
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

        assert_eq!(store.codex_quota.unwrap().primary_pct, 24.0);
        assert_eq!(store.codex_spark_quota.unwrap().primary_pct, 7.0);
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
