//! Plugin (skill) manifests — how a GitHub repo becomes "installable".
//!
//! The format is deliberately close to Claude Code's `.claude-plugin/plugin.json`
//! and Claude/OpenAI "skills", so existing public repos can be installed by
//! pointing the app at them, while staying trivial to validate on the phone.
//!
//! A plugin is a directory in the repo:
//!
//! ```text
//! my-plugin/
//!   turbollm.plugin.json      <- this manifest (required)
//!   skills/<name>/SKILL.md    <- optional: extra instructions (front-matter + body)
//!   README.md
//! ```
//!
//! Manifest:
//!
//! ```json
//! {
//!   "name": "web-tools",
//!   "version": "1.0.0",
//!   "description": "Fetch a URL and summarise it",
//!   "author": "you",
//!   "instructions": "Use web_fetch for any URL the user mentions.",
//!   "system_prompt": "Ты — ассистент с доступом к вебу.",
//!   "skills": ["skills/summarize"],
//!   "mcp_servers": [
//!     {"name": "fetcher", "url": "https://example.com/mcp"},
//!     {"name": "local", "command": "python3", "args": ["-m", "my_server"]}
//!   ],
//!   "tools": [
//!     {"name": "clock", "description": "current time",
//!      "parameters": {"type": "object", "properties": {}},
//!      "http": {"method": "GET", "url": "https://worldtimeapi.org/api/ip"}}
//!   ]
//! }
//! ```

use serde::{Deserialize, Serialize};

use crate::mcp::ServerConfig;
use crate::tools::ToolSpec;

/// A tool that the app itself executes over HTTP (no MCP server needed).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HttpTool {
    #[serde(default = "default_get")]
    pub method: String,
    /// May contain `{param}` placeholders that are filled from the arguments.
    pub url: String,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    /// Optional body template with `{param}` placeholders.
    #[serde(default)]
    pub body: String,
    /// Truncate the response before handing it to the model.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
}

fn default_get() -> String {
    "GET".into()
}
fn default_max_bytes() -> usize {
    8_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    /// Extra text appended to the system prompt (skill instructions).
    #[serde(default)]
    pub instructions: String,
    /// Replaces the default system prompt when set.
    #[serde(default)]
    pub system_prompt: String,
    /// Paths to `SKILL.md` files inside the repo.
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<ServerConfig>,
    /// Local (app-executed) tools; the HTTP action is optional.
    #[serde(default)]
    pub tools: Vec<PluginTool>,
    #[serde(default)]
    pub homepage: String,
    /// Minimum app version that can install this plugin.
    #[serde(default)]
    pub min_app_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_schema")]
    pub parameters: serde_json::Value,
    #[serde(default)]
    pub http: Option<HttpTool>,
}

fn default_schema() -> serde_json::Value {
    serde_json::json!({"type": "object", "properties": {}})
}

impl PluginTool {
    pub fn to_spec(&self, plugin: &str) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
            kind: if self.http.is_some() {
                "http".to_string()
            } else {
                "local".to_string()
            },
            server: plugin.to_string(),
        }
    }
}

/// An installed plugin as stored on the device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPlugin {
    pub manifest: PluginManifest,
    /// `owner/repo` or `owner/repo@ref` it came from.
    pub source: String,
    /// Absolute path of the unpacked plugin directory.
    pub path: String,
    #[serde(default)]
    pub enabled: bool,
    /// Concatenated SKILL.md bodies (already stripped of front matter).
    pub skill_text: String,
}

impl InstalledPlugin {
    pub fn tools(&self) -> Vec<ToolSpec> {
        self.manifest
            .tools
            .iter()
            .map(|t| t.to_spec(&self.manifest.name))
            .collect()
    }

    /// What gets appended to the system prompt when the plugin is on.
    pub fn prompt_addendum(&self) -> String {
        let mut out = String::new();
        if !self.manifest.instructions.trim().is_empty() {
            out.push_str("\n\n");
            out.push_str(self.manifest.instructions.trim());
        }
        if !self.skill_text.trim().is_empty() {
            out.push_str("\n\n# Skills\n\n");
            out.push_str(self.skill_text.trim());
        }
        out
    }
}

/// Parse `owner/repo`, `owner/repo@v1.2`, a full GitHub URL or a subdirectory
/// (`owner/repo/path/to/plugin`).
#[derive(Debug, Clone, PartialEq)]
pub struct GitHubRef {
    pub owner: String,
    pub repo: String,
    pub git_ref: Option<String>,
    pub subdir: Option<String>,
}

impl GitHubRef {
    pub fn parse(input: &str) -> Result<Self, String> {
        let raw = input.trim();
        let raw = raw
            .strip_prefix("https://github.com/")
            .or_else(|| raw.strip_prefix("http://github.com/"))
            .or_else(|| raw.strip_prefix("github.com/"))
            .unwrap_or(raw)
            .trim_end_matches('/');
        let raw = raw.strip_suffix(".git").unwrap_or(raw);
        if raw.is_empty() {
            return Err("пустая ссылка".into());
        }
        // `owner/repo@v1/plugins/weather`: the ref ends at the first '/'.
        let (path, git_ref, extra_subdir) = match raw.split_once('@') {
            Some((p, r)) => match r.split_once('/') {
                Some((git_ref, tail)) => (p, Some(git_ref.to_string()), Some(tail.to_string())),
                None => (p, Some(r.to_string()), None),
            },
            None => (raw, None, None),
        };
        let mut parts = path.split('/');
        let owner = parts.next().unwrap_or_default();
        let repo = parts.next().unwrap_or_default();
        if owner.is_empty() || repo.is_empty() {
            return Err(format!("ожидалось owner/repo, получено «{input}»"));
        }
        let mut subdir: Vec<&str> = parts.filter(|p| !p.is_empty()).collect();
        if let Some(extra) = &extra_subdir {
            subdir.insert(0, extra.as_str());
        }
        Ok(Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
            git_ref,
            subdir: if subdir.is_empty() {
                None
            } else {
                Some(subdir.join("/"))
            },
        })
    }

    /// Default manifest name inside a plugin folder.
    pub const MANIFEST_FILE: &'static str = "turbollm.plugin.json";

    /// URLs the app tries, in order: the manifest itself first, then the repo
    /// README/listing via the API to give a helpful error message.
    pub fn manifest_urls(&self) -> Vec<String> {
        let mut urls = Vec::new();
        let base_dirs: Vec<String> = match &self.subdir {
            Some(d) => vec![
                d.clone(),
                ".claude-plugin".to_string(),
                format!("{d}/.claude-plugin"),
            ],
            None => vec![
                String::new(),
                ".claude-plugin".to_string(),
                "plugins".to_string(),
            ],
        };
        for dir in base_dirs {
            for name in [Self::MANIFEST_FILE, "plugin.json"] {
                let path = if dir.is_empty() {
                    name.to_string()
                } else {
                    format!("{dir}/{name}")
                };
                urls.push(self.raw_url(&path));
            }
        }
        urls
    }

    /// `https://raw.githubusercontent.com/owner/repo/<ref>/<path>`
    pub fn raw_url(&self, path: &str) -> String {
        let r = self.git_ref.clone().unwrap_or_else(|| "HEAD".to_string());
        format!(
            "https://raw.githubusercontent.com/{}/{}/{}/{}",
            self.owner, self.repo, r, path
        )
    }

    /// Fallback: download the whole repo as a zip (used when the manifest pulls
    /// in skills or needs sibling files).
    pub fn archive_url(&self) -> String {
        match &self.git_ref {
            Some(r) => format!(
                "https://github.com/{}/{}/archive/{}.zip",
                self.owner, self.repo, r
            ),
            None => format!(
                "https://github.com/{}/{}/archive/HEAD.zip",
                self.owner, self.repo
            ),
        }
    }
}

/// Strip YAML front matter from a `SKILL.md` and return the body.
pub fn strip_front_matter(md: &str) -> String {
    let t = md.trim_start();
    if let Some(rest) = t.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            return rest[end + 4..].trim_start().to_string();
        }
    }
    t.to_string()
}

/// Validate a manifest and produce readable problems.
pub fn validate(m: &PluginManifest) -> Vec<String> {
    let mut errs = Vec::new();
    if m.name.trim().is_empty() {
        errs.push("поле «name» обязательно".to_string());
    }
    if m.name.len() > 48 {
        errs.push("«name» длиннее 48 символов".to_string());
    }
    if !m
        .name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        errs.push("«name» может содержать только [a-z0-9-_]".to_string());
    }
    for t in &m.tools {
        if t.name.trim().is_empty() {
            errs.push("у инструмента нет имени".to_string());
        }
        if let Some(http) = &t.http {
            if !(http.url.starts_with("https://") || http.url.starts_with("http://")) {
                errs.push(format!(
                    "инструмент {}: url должен начинаться с https://",
                    t.name
                ));
            }
            if http.url.starts_with("http://") {
                errs.push(format!(
                    "инструмент {}: http:// запрещён (Android блокирует cleartext)",
                    t.name
                ));
            }
        }
    }
    for s in &m.mcp_servers {
        if s.name.trim().is_empty() {
            errs.push("у MCP-сервера нет имени".to_string());
        }
        if s.url.is_empty() && s.command.is_empty() {
            errs.push(format!(
                "MCP-сервер {}: нужен либо url, либо command",
                s.name
            ));
        }
        if !s.url.is_empty() && s.url.starts_with("http://") {
            errs.push(format!("MCP-сервер {}: http:// запрещён", s.name));
        }
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_refs() {
        let r = GitHubRef::parse("https://github.com/anthropics/claude-code").unwrap();
        assert_eq!(
            (r.owner.as_str(), r.repo.as_str()),
            ("anthropics", "claude-code")
        );
        assert!(r.subdir.is_none());

        let r = GitHubRef::parse("me/my-plugins@v1/plugins/weather").unwrap();
        assert_eq!(r.git_ref.as_deref(), Some("v1"));
        assert_eq!(r.subdir.as_deref(), Some("plugins/weather"));

        assert!(GitHubRef::parse("nonsense").is_err());
    }

    #[test]
    fn builds_raw_urls() {
        let r = GitHubRef::parse("owner/repo@dev/docs/plugin").unwrap();
        let urls = r.manifest_urls();
        assert!(urls[0].contains("/dev/docs/plugin/turbollm.plugin.json"));
        assert!(urls.iter().any(|u| u.contains(".claude-plugin")));
        assert!(r.archive_url().ends_with("/archive/dev.zip"));
    }

    #[test]
    fn validates_manifest() {
        let m: PluginManifest = serde_json::from_str(
            r#"{"name":"web","tools":[{"name":"f","http":{"method":"GET","url":"https://x.y"}}]}"#,
        )
        .unwrap();
        assert!(validate(&m).is_empty());

        let bad: PluginManifest = serde_json::from_str(
            r#"{"name":"web tools","tools":[{"name":"f","http":{"url":"http://x.y"}}]}"#,
        )
        .unwrap();
        let errs = validate(&bad);
        assert!(errs.iter().any(|e| e.contains("только [a-z0-9-_]")));
        assert!(errs.iter().any(|e| e.contains("cleartext")));
    }

    #[test]
    fn strips_skill_front_matter() {
        let md = "---\nname: x\ndescription: y\n---\n\n# Body\nUse it.";
        assert_eq!(strip_front_matter(md), "# Body\nUse it.");
        assert_eq!(strip_front_matter("no front matter"), "no front matter");
    }

    #[test]
    fn tool_specs_and_prompt() {
        let m: PluginManifest = serde_json::from_str(
            r#"{"name":"weather","instructions":"Always use weather.",
                "tools":[{"name":"weather","description":"get weather"}]}"#,
        )
        .unwrap();
        let p = InstalledPlugin {
            manifest: m,
            source: "me/x".into(),
            path: "/tmp".into(),
            enabled: true,
            skill_text: "# Skill\nBe brief.".into(),
        };
        let specs = p.tools();
        assert_eq!(specs[0].kind, "local");
        let add = p.prompt_addendum();
        assert!(add.contains("Always use weather."));
        assert!(add.contains("Be brief."));
    }
}
