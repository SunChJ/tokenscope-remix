// Loads the "user-installed" whitelists so the dashboard only counts
// MCP servers / Skills the user actually added (PRD decision).
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub struct UserConfig {
    pub mcp_servers: HashSet<String>,       // claude, from ~/.claude.json
    pub codex_mcp_servers: HashSet<String>, // codex, from ~/.codex/config.toml
    pub skills: HashSet<String>,
}

fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

/// Parse ~/.claude.json once (None if missing/unreadable/invalid).
fn read_user_config() -> Option<serde_json::Value> {
    let path = home()?.join(".claude.json");
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// mcpServers (top level) + projects[*].mcpServers from a parsed ~/.claude.json.
fn mcps_from(json: Option<&serde_json::Value>) -> HashSet<String> {
    let mut set = HashSet::new();
    let Some(json) = json else { return set };
    if let Some(obj) = json.get("mcpServers").and_then(|v| v.as_object()) {
        for k in obj.keys() {
            set.insert(k.clone());
        }
    }
    if let Some(projects) = json.get("projects").and_then(|v| v.as_object()) {
        for proj in projects.values() {
            if let Some(obj) = proj.get("mcpServers").and_then(|v| v.as_object()) {
                for k in obj.keys() {
                    set.insert(k.clone());
                }
            }
        }
    }
    set
}

/// Add each subdirectory name of `dir` to the set (skills are folders).
fn scan_skill_dir(dir: &Path, set: &mut HashSet<String>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                if let Some(name) = e.file_name().to_str() {
                    set.insert(name.to_string());
                }
            }
        }
    }
}

/// User-installed skills = global ~/.claude/skills/ only (PRD §3.3). Project-
/// level skill dirs are intentionally not scanned: the PRD defines the skill
/// source as the global directory, and folding in every registered project's
/// dir inflated the "installed skills" metric.
fn load_user_skills() -> HashSet<String> {
    let mut set = HashSet::new();
    if let Some(h) = home() {
        scan_skill_dir(&h.join(".claude").join("skills"), &mut set);
    }
    set
}

/// Codex normalizes MCP server names to snake_case in tool names (a config
/// entry `chrome-devtools` calls tools named `mcp__chrome_devtools__…`), so
/// both the whitelist and lookups go through this.
fn norm_mcp(name: &str) -> String {
    name.replace('-', "_")
}

/// MCP servers from ~/.codex/config.toml (honoring CODEX_HOME): every
/// `[mcp_servers.<name>]` table header. A structural TOML parse isn't needed
/// for section names, so we scan headers instead of adding a toml dependency.
fn load_codex_mcps() -> HashSet<String> {
    let mut set = HashSet::new();
    let path = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".codex")))
        .map(|d| d.join("config.toml"));
    let Some(text) = path.and_then(|p| fs::read_to_string(p).ok()) else {
        return set;
    };
    for line in text.lines() {
        let line = line.trim();
        // Match [mcp_servers.<name>] and [mcp_servers."<name>"], but not deeper
        // sub-tables like [mcp_servers.<name>.env].
        let Some(rest) = line.strip_prefix("[mcp_servers.") else { continue };
        let Some(inner) = rest.strip_suffix(']') else { continue };
        let name = inner.trim_matches('"');
        if !name.is_empty() && !name.contains('.') {
            set.insert(norm_mcp(name));
        }
    }
    set
}

impl UserConfig {
    pub fn load() -> Self {
        // Parse ~/.claude.json a single time and derive the MCP whitelist from it.
        let json = read_user_config();
        UserConfig {
            mcp_servers: mcps_from(json.as_ref()),
            codex_mcp_servers: load_codex_mcps(),
            skills: load_user_skills(),
        }
    }

    /// A tool name like "mcp__<server>__<tool>" → is server user-installed?
    /// Checked against the owning agent's own config.
    pub fn is_user_mcp(&self, agent: &str, server: &str) -> bool {
        if agent == crate::store::AGENT_CODEX {
            self.codex_mcp_servers.contains(&norm_mcp(server))
        } else {
            self.mcp_servers.contains(server)
        }
    }

    /// A skill id (may be "plugin:skill") → strip plugin prefix, check dir.
    pub fn is_user_skill(&self, skill: &str) -> bool {
        let key = skill.rsplit(':').next().unwrap_or(skill);
        self.skills.contains(key)
    }
}
