// Incremental event store.
//
// Ingestion (this file) is the only place that touches the JSONL logs. It
// parses each log line into a provider/config/price-independent RawEvent
// (just the facts), reads only newly-appended bytes of changed files (tracked
// by a per-file manifest), dedupes by message id, and persists everything to
// the cache dir. Aggregation (parser.rs) then works purely on these in-memory
// events — cheap, and recomputed per request because the Day/Week/Month
// windows are relative to "now".
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
    pub skills: Vec<String>, // all Skill input.skill ids called (unfiltered)
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
    // Latest Codex rate-limit snapshot seen across all session files.
    pub codex_quota: Option<Quota>,
}

// Bump when the parsing/extraction logic changes in a way that requires
// re-reading logs from scratch (the incremental manifest would otherwise skip
// already-seen bytes and miss newly-extracted facts).
//   v2: count slash-command skill invocations (`/skill`), not just Skill tool_use.
//   v3: merge tool_use across lines sharing a message id (a thinking line + a
//       tool_use line were deduped, dropping the tool call).
//   v4: track a per-event source file (idempotent re-read of truncated logs).
//   v5: multi-agent ingest (claude + codex), FileState manifest, quota snapshot.
const STORE_VERSION: u32 = 5;

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
        if let Some(dir) = cache_dir() {
            // If the cache was written by an older parser, discard it so ingest
            // does a full rescan and picks up newly-extracted facts.
            let version_ok = fs::read_to_string(dir.join("version"))
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
                // Quota is a best-effort side channel — a missing/corrupt file
                // just means "no snapshot yet", never a rescan.
                codex_quota = fs::read_to_string(dir.join("codex_quota.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str::<Quota>(&t).ok());
            }
        }
        let index = events
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.id.is_empty())
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        Store {
            events,
            index,
            manifest,
            codex_quota,
        }
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
            if let Some(q) = &self.codex_quota {
                if let Ok(t) = serde_json::to_string(q) {
                    let _ = write_atomic(&dir.join("codex_quota.json"), t.as_bytes());
                }
            }
            let _ = write_atomic(&dir.join("version"), STORE_VERSION.to_string().as_bytes());
        }
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

    /// Drop events older than `cutoff_ms`. The reports/heatmap only span the last
    /// ~26 weeks, so anything older is dead weight that grows events.json without
    /// bound. Returns whether anything was removed. Old logs already at EOF are
    /// never re-read, so their pruned events don't reappear.
    pub fn prune_before(&mut self, cutoff_ms: i64) -> bool {
        let before = self.events.len();
        self.events.retain(|e| e.ts_ms >= cutoff_ms);
        let removed = self.events.len() != before;
        if removed {
            self.rebuild_index();
        }
        removed
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
                None
            }
            "turn_context" => {
                if let Some(m) = payload.get("model").and_then(|x| x.as_str()) {
                    state.model = m.to_string();
                }
                None
            }
            "event_msg" => {
                if payload.get("type")?.as_str()? != "token_count" {
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
                // MCP tool calls: function_call items named mcp__<server>[__<tool>].
                if payload.get("type")?.as_str()? != "function_call" {
                    return None;
                }
                let name = payload.get("name")?.as_str()?;
                let rest = name.strip_prefix("mcp__")?;
                let server = rest.split("__").next().unwrap_or("").to_string();
                if server.is_empty() {
                    return None;
                }
                Some(RawEvent {
                    ts_ms: parse_ts(&v)?,
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
        if self.codex_quota.as_ref().map(|q| q.as_of_ms >= ts_ms).unwrap_or(false) {
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
        self.codex_quota = Some(Quota {
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
