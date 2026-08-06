// Loads the "user-installed" whitelists so the dashboard only counts
// MCP servers / Skills the user actually added (PRD decision).
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub struct UserConfig {
    pub mcp_servers: HashSet<String>,       // claude, from ~/.claude.json
    pub codex_mcp_servers: HashSet<String>, // codex, from ~/.codex/config.toml
    pub claude_skills: HashSet<String>,
    pub codex_skills: HashSet<String>,
    pub pi_skills: HashSet<String>,
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
fn scan_skill_dir(dir: &Path, set: &mut HashSet<String>, include_hidden: bool) {
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                if let Some(name) = e.file_name().to_str() {
                    if (include_hidden || !name.starts_with('.'))
                        && e.path().join("SKILL.md").is_file()
                    {
                        set.insert(name.to_string());
                    }
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
        scan_skill_dir(&h.join(".claude").join("skills"), &mut set, true);
    }
    set
}

fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".codex")))
}

fn pi_agent_dir() -> Option<PathBuf> {
    std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".pi").join("agent")))
}

/// User Codex skills live in $CODEX_HOME/skills and ~/.agents/skills. Also
/// include project `.agents/skills` directories from session working dirs.
/// Hidden directories such as $CODEX_HOME/skills/.system are built-ins.
fn load_codex_skills(project_dirs: &[PathBuf]) -> HashSet<String> {
    let mut set = HashSet::new();
    let mut dirs = HashSet::new();
    if let Some(d) = codex_home() {
        dirs.insert(d.join("skills"));
    }
    if let Some(h) = home() {
        dirs.insert(h.join(".agents").join("skills"));
    }
    for project in project_dirs {
        for dir in project.ancestors() {
            dirs.insert(dir.join(".agents").join("skills"));
        }
    }
    for dir in dirs {
        scan_skill_dir(&dir, &mut set, false);
    }
    set
}

fn pi_frontmatter_name(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if let Some(name) = line.strip_prefix("name:") {
            let name = name.trim().trim_matches(['\'', '"']);
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn scan_pi_skill_path(path: &Path, set: &mut HashSet<String>) {
    if path.is_file() {
        if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            if let Some(name) = pi_frontmatter_name(path).or_else(|| {
                path.file_stem()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
            }) {
                set.insert(name);
            }
        }
        return;
    }
    for entry in walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_type().is_file() && entry.file_name() == "SKILL.md" {
            if let Some(parent_name) = entry
                .path()
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
            {
                set.insert(parent_name.to_string());
            }
            if let Some(name) = pi_frontmatter_name(entry.path()) {
                set.insert(name);
            }
        }
    }
}

fn expand_pi_path(path: &str, base: &Path) -> Option<PathBuf> {
    if path == "~" {
        return home();
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return Some(home()?.join(rest));
    }
    let path = PathBuf::from(path);
    Some(if path.is_absolute() { path } else { base.join(path) })
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn add_pi_settings_skills(settings: &Path, base: &Path, set: &mut HashSet<String>) {
    let Some(json) = read_json(settings) else {
        return;
    };
    let Some(skills) = json.get("skills").and_then(|value| value.as_array()) else {
        return;
    };
    for skill in skills.iter().filter_map(|value| value.as_str()) {
        if skill.starts_with(['!', '-', '+']) || skill.contains('*') {
            continue;
        }
        if let Some(path) = expand_pi_path(skill, base) {
            scan_pi_skill_path(&path, set);
        }
    }
}

fn npm_package_name(source: &str) -> &str {
    let source = source.strip_prefix("npm:").unwrap_or(source);
    if let Some(unscoped) = source.strip_prefix('@') {
        unscoped
            .find('@')
            .map(|index| &source[..index + 1])
            .unwrap_or(source)
    } else {
        source.split('@').next().unwrap_or(source)
    }
}

fn pi_package_path(source: &str, base: &Path) -> Option<PathBuf> {
    let source = source.split('#').next()?.trim();
    if let Some(repo) = source.strip_prefix("git:") {
        return Some(base.join("git").join(repo));
    }
    if let Some(repo) = source.strip_prefix("github:") {
        return Some(base.join("git").join("github.com").join(repo));
    }
    if source.contains("://") {
        return None;
    }
    let name = npm_package_name(source);
    (!name.is_empty()).then(|| base.join("npm").join("node_modules").join(name))
}

fn add_pi_package_skills(settings: &Path, base: &Path, set: &mut HashSet<String>) {
    let Some(json) = read_json(settings) else {
        return;
    };
    let Some(packages) = json.get("packages").and_then(|value| value.as_array()) else {
        return;
    };
    for package in packages {
        let source = if let Some(source) = package.as_str() {
            source
        } else {
            let skills_enabled = package
                .get("skills")
                .and_then(|value| value.as_array())
                .is_none_or(|skills| !skills.is_empty());
            if !skills_enabled {
                continue;
            }
            let Some(source) = package.get("source").and_then(|value| value.as_str()) else {
                continue;
            };
            source
        };
        if let Some(path) = pi_package_path(source, base) {
            scan_pi_skill_path(&path, set);
        }
    }
}

/// Pi discovers global, shared, project, package, and explicit settings skills.
fn load_pi_skills(project_dirs: &[PathBuf]) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Some(agent_dir) = pi_agent_dir() {
        scan_pi_skill_path(&agent_dir.join("skills"), &mut set);
        let settings = agent_dir.join("settings.json");
        add_pi_settings_skills(&settings, &agent_dir, &mut set);
        add_pi_package_skills(&settings, &agent_dir, &mut set);
    }
    if let Some(home) = home() {
        scan_pi_skill_path(&home.join(".agents").join("skills"), &mut set);
    }
    for project in project_dirs {
        for dir in project.ancestors() {
            scan_pi_skill_path(&dir.join(".pi").join("skills"), &mut set);
            scan_pi_skill_path(&dir.join(".agents").join("skills"), &mut set);
            let settings_dir = dir.join(".pi");
            let settings = settings_dir.join("settings.json");
            add_pi_settings_skills(&settings, &settings_dir, &mut set);
            add_pi_package_skills(&settings, &settings_dir, &mut set);
            if dir.join(".git").exists() {
                break;
            }
        }
    }
    set
}

/// Codex normalizes MCP server names to snake_case in tool names (a config
/// entry `chrome-devtools` calls tools named `mcp__chrome_devtools__…`), so
/// both the whitelist and lookups go through this.
fn norm_mcp(name: &str) -> String {
    name.replace('-', "_")
}

/// Collect every `[mcp_servers.<name>]` table header. A structural TOML parse
/// isn't needed for section names, so keep this dependency-free.
fn add_codex_mcps(text: &str, set: &mut HashSet<String>) {
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
}

/// MCP servers from the global Codex config plus trusted project configs seen
/// in session working directories.
fn load_codex_mcps(project_dirs: &[PathBuf]) -> HashSet<String> {
    let mut set = HashSet::new();
    let mut paths = HashSet::new();
    if let Some(d) = codex_home() {
        paths.insert(d.join("config.toml"));
    }
    for project in project_dirs {
        for dir in project.ancestors() {
            paths.insert(dir.join(".codex").join("config.toml"));
        }
    }
    for path in paths {
        if let Ok(text) = fs::read_to_string(path) {
            add_codex_mcps(&text, &mut set);
        }
    }
    set
}

impl UserConfig {
    pub fn load(project_dirs: &[PathBuf]) -> Self {
        // Parse ~/.claude.json a single time and derive the MCP whitelist from it.
        let json = read_user_config();
        UserConfig {
            mcp_servers: mcps_from(json.as_ref()),
            codex_mcp_servers: load_codex_mcps(project_dirs),
            claude_skills: load_user_skills(),
            codex_skills: load_codex_skills(project_dirs),
            pi_skills: load_pi_skills(project_dirs),
        }
    }

    /// A tool name like "mcp__<server>__<tool>" → is server user-installed?
    /// Checked against the owning agent's own config.
    pub fn is_user_mcp(&self, agent: &str, server: &str) -> bool {
        match agent {
            crate::store::AGENT_CODEX => self.codex_mcp_servers.contains(&norm_mcp(server)),
            // Pi has no built-in MCP registry. A persisted mcp__ tool is supplied
            // by a user extension, so the invocation itself is authoritative.
            crate::store::AGENT_PI => !server.is_empty(),
            _ => self.mcp_servers.contains(server),
        }
    }

    /// A skill id (may be "plugin:skill") → strip plugin prefix, check dir.
    pub fn is_user_skill(&self, agent: &str, skill: &str) -> bool {
        let key = skill.rsplit(':').next().unwrap_or(skill);
        match agent {
            crate::store::AGENT_CODEX => self.codex_skills.contains(key),
            crate::store::AGENT_PI => self.pi_skills.contains(key),
            _ => self.claude_skills.contains(key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_pi_package_install_paths() {
        let base = Path::new("/agent");
        assert_eq!(npm_package_name("npm:toolkit@1.2.3"), "toolkit");
        assert_eq!(npm_package_name("npm:@scope/toolkit@1.2.3"), "@scope/toolkit");
        assert_eq!(
            pi_package_path("npm:@scope/toolkit@1.2.3", base),
            Some(base.join("npm/node_modules/@scope/toolkit"))
        );
        assert_eq!(
            pi_package_path("git:github.com/org/toolkit#main", base),
            Some(base.join("git/github.com/org/toolkit"))
        );
    }

    #[test]
    fn parses_and_normalizes_codex_mcp_server_names() {
        let mut servers = HashSet::new();
        add_codex_mcps(
            r#"
                [mcp_servers.chrome-devtools]
                [mcp_servers."node_repl"]
                [mcp_servers.chrome-devtools.env]
            "#,
            &mut servers,
        );
        assert_eq!(
            servers,
            HashSet::from(["chrome_devtools".into(), "node_repl".into()])
        );
    }
}
