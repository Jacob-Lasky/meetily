//! Settings › AI Assist (WORK-581): the model, the standing context (about
//! me, knowledge), extra instructions, which card kinds to show, the MCP
//! servers the research lane may use, and this call's meeting brief.
//!
//! Stored in the `live_assist.json` Tauri store next to the `enabled` key.
//! DO NOT move any of it into the database: a migration this build adds
//! cannot be opened by the previous AppImage (sqlx refuses a DB with unknown
//! applied migrations), and rolling back to that build is the documented
//! recovery path. The older build only reads `enabled` and ignores these keys.

use serde::{Deserialize, Serialize};

use super::cards::CardKind;

/// Store key of the settings object.
pub const SETTINGS_KEY: &str = "settings";
/// Store key of the meeting brief (kept until the recording that used it stops).
pub const BRIEF_KEY: &str = "brief";

/// Most characters of standing context (about me + knowledge + brief) sent
/// with each call. Claude caches it, but it still has to fit the context
/// window and it is re-sent (and re-billed at the cache rate) on every call:
/// 120k characters is about 30k tokens.
pub const MAX_CONTEXT_CHARS: usize = 120_000;
/// Most characters of extra instructions.
pub const MAX_INSTRUCTIONS_CHARS: usize = 4_000;
/// Most MCP servers.
pub const MAX_MCP_SERVERS: usize = 8;

/// A model just for assist cards, instead of the summary model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOverride {
    /// "claude", "openai", "groq", "openrouter", "ollama", "custom-openai".
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeItem {
    pub id: String,
    /// A label, or the file name it was loaded from.
    pub name: String,
    pub text: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CardKinds {
    pub answer: bool,
    pub ask: bool,
    pub fact: bool,
    pub todo: bool,
}

impl Default for CardKinds {
    fn default() -> Self {
        Self { answer: true, ask: true, fact: true, todo: true }
    }
}

impl CardKinds {
    pub fn allows(&self, kind: CardKind) -> bool {
        match kind {
            CardKind::Answer => self.answer,
            CardKind::Ask => self.ask,
            CardKind::Fact => self.fact,
            CardKind::Todo => self.todo,
        }
    }

    /// The enabled kinds, in the prompt's order.
    pub fn enabled(&self) -> Vec<CardKind> {
        [CardKind::Answer, CardKind::Ask, CardKind::Fact, CardKind::Todo]
            .into_iter()
            .filter(|k| self.allows(*k))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    /// A local command speaking MCP on stdin/stdout.
    Stdio,
    /// Streamable HTTP.
    Http,
    /// Legacy SSE.
    Sse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServer {
    pub id: String,
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub transport: McpTransport,
    /// stdio: the program and its arguments.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// stdio: extra environment variables.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// http / sse: the endpoint and request headers.
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    /// Tool names the research lane may call. Empty: every tool the server lists.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Research {
    /// The research lane runs (Pipecat engine only). Off: no MCP connection
    /// is opened and the card prompt does not offer research.
    pub enabled: bool,
    pub servers: Vec<McpServer>,
}

impl Default for Research {
    /// On by default since the built-in Deepgram docs server (WORK-581): a
    /// fresh install gets docs-grounded lookups with nothing to set up.
    fn default() -> Self {
        Self { enabled: true, servers: Vec::new() }
    }
}

/// The built-in Deepgram knowledge (`scripts/refresh-deepgram-knowledge.py`
/// regenerates it from deepgram.com/llms.txt).
pub const DEEPGRAM_PUBLIC: &str = include_str!("deepgram_public.md");

/// Name of the built-in Deepgram docs research server; user servers cannot
/// take it (the sidecar keys MCP clients by server name).
pub const BUILTIN_DOCS_NAME: &str = "Deepgram docs";
/// Names of the built-in Pipecat and Daily docs servers (also reserved).
pub const BUILTIN_PIPECAT_DOCS_NAME: &str = "Pipecat docs";
pub const BUILTIN_DAILY_DOCS_NAME: &str = "Daily docs";
/// Every name user servers cannot take.
pub const RESERVED_SERVER_NAMES: [&str; 3] = [BUILTIN_DOCS_NAME, BUILTIN_PIPECAT_DOCS_NAME, BUILTIN_DAILY_DOCS_NAME];

/// Knowledge and research that ship with the app (WORK-581), so colleagues
/// get Deepgram-grounded cards with nothing to paste or configure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Builtin {
    /// `deepgram_public.md` (from deepgram.com/llms.txt) as a knowledge item.
    pub deepgram_overview: bool,
    /// Deepgram's hosted docs MCP server as a research server, authenticated
    /// with the Deepgram API key saved for transcription.
    pub deepgram_docs: bool,
    /// Pipecat's and Daily's public docs MCP servers (docs.pipecat.ai/mcp,
    /// docs.daily.co/mcp; no sign-in), search tools only.
    pub pipecat_daily_docs: bool,
}

impl Default for Builtin {
    fn default() -> Self {
        Self { deepgram_overview: true, deepgram_docs: true, pipecat_daily_docs: true }
    }
}

impl Research {
    /// Servers the lane would use: research on, server on.
    pub fn active_servers(&self) -> Vec<&McpServer> {
        if !self.enabled {
            return Vec::new();
        }
        self.servers.iter().filter(|s| s.enabled).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct AssistSettings {
    /// None: the summary model (Settings › Summary).
    pub model: Option<ModelOverride>,
    pub about_me: String,
    pub knowledge: Vec<KnowledgeItem>,
    /// Added after the built-in rules; they never replace them.
    pub instructions: String,
    pub card_kinds: CardKinds,
    pub research: Research,
    pub builtin: Builtin,
}

fn yes() -> bool {
    true
}

impl AssistSettings {
    /// About me + enabled knowledge, in characters.
    fn own_context_chars(&self) -> usize {
        self.about_me.chars().count()
            + self.knowledge.iter().filter(|k| k.enabled).map(|k| k.text.chars().count()).sum::<usize>()
    }

    /// The built-in overview goes into the prompt: it is on, and it fits
    /// under the context limit next to about me and the enabled knowledge
    /// (settings saved before it existed may already fill the limit; then
    /// it is left out rather than going over).
    ///
    /// `brief_chars` is the meeting brief's length, which shares the limit.
    pub fn overview_included(&self, brief_chars: usize) -> bool {
        self.builtin.deepgram_overview
            && self.own_context_chars() + brief_chars + DEEPGRAM_PUBLIC.chars().count() <= MAX_CONTEXT_CHARS
    }

    /// About me + enabled knowledge, plus the built-in overview when it
    /// goes into the prompt (`overview_included`), in characters: what
    /// `validate` holds to MAX_CONTEXT_CHARS. The overview never makes a save
    /// fail: when it does not fit, it is left out instead.
    ///
    /// `brief_chars` (the meeting brief) is included in the total.
    pub fn context_chars(&self, brief_chars: usize) -> usize {
        let builtin = if self.overview_included(brief_chars) { DEEPGRAM_PUBLIC.chars().count() } else { 0 };
        builtin + self.own_context_chars() + brief_chars
    }

    /// Settings read from the store. An object saved before the built-ins
    /// existed (no `builtin` key) and with no MCP servers of its own gets
    /// research on, the new default: with no server, off could only have
    /// been the old default, and turning it on reaches only the built-in
    /// docs. One that lists servers keeps its choice: on would start sending
    /// lookups to them.
    pub fn from_stored(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        let had_builtin = value.get("builtin").is_some();
        // Saved before the Pipecat and Daily docs existed: follow the choice
        // made for the other built-in public docs (Deepgram's), so a setup
        // that turned built-in docs off does not gain new public servers.
        let had_pipecat_daily = value.pointer("/builtin/pipecatDailyDocs").is_some();
        let mut settings: Self = serde_json::from_value(value)?;
        if !had_builtin && settings.research.servers.is_empty() {
            settings.research.enabled = true;
        }
        if had_builtin && !had_pipecat_daily {
            settings.builtin.pipecat_daily_docs = settings.builtin.deepgram_docs;
        }
        Ok(settings)
    }

    /// Check a settings object before it is saved. `brief_chars` is the
    /// meeting brief's length, which shares the context limit. Errors are
    /// shown as-is.
    pub fn validate(&self, brief_chars: usize) -> Result<(), String> {
        if self.card_kinds.enabled().is_empty() {
            return Err("turn on at least one card kind".into());
        }
        if let Some(m) = &self.model {
            if m.provider.trim().is_empty() || m.model.trim().is_empty() {
                return Err("the assist model needs both a provider and a model".into());
            }
            use crate::summary::llm_client::LLMProvider;
            if LLMProvider::from_str(&m.provider)? == LLMProvider::BuiltInAI {
                return Err("the built-in local model is too slow for live cards; pick a cloud model".into());
            }
        }
        if self.instructions.chars().count() > MAX_INSTRUCTIONS_CHARS {
            return Err(format!("instructions are limited to {MAX_INSTRUCTIONS_CHARS} characters"));
        }
        let context = self.context_chars(brief_chars);
        if context > MAX_CONTEXT_CHARS {
            return Err(format!(
                "about me, the enabled knowledge and the meeting brief come to {context} characters; the limit is {MAX_CONTEXT_CHARS}"
            ));
        }
        if self.research.servers.len() > MAX_MCP_SERVERS {
            return Err(format!("at most {MAX_MCP_SERVERS} MCP servers"));
        }
        let mut names = std::collections::HashSet::new();
        for s in &self.research.servers {
            if s.name.trim().is_empty() {
                return Err("every MCP server needs a name".into());
            }
            if let Some(reserved) = RESERVED_SERVER_NAMES.iter().find(|n| s.name.trim().eq_ignore_ascii_case(n)) {
                return Err(format!("{reserved} is a built-in server's name; pick another"));
            }
            if !names.insert(s.name.trim().to_lowercase()) {
                return Err(format!("two MCP servers are named {}", s.name.trim()));
            }
            match s.transport {
                McpTransport::Stdio if s.command.trim().is_empty() => {
                    return Err(format!("MCP server {}: a local server needs a command", s.name));
                }
                McpTransport::Http | McpTransport::Sse => {
                    let url = s.url.trim();
                    if !(url.starts_with("https://") || url.starts_with("http://")) {
                        return Err(format!("MCP server {}: the URL must start with https:// or http://", s.name));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_store_object_is_the_default() {
        let s: AssistSettings = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(s, AssistSettings::default());
        assert!(s.card_kinds.answer && s.card_kinds.todo);
        assert!(s.builtin.deepgram_overview && s.builtin.deepgram_docs && s.research.enabled, "built-ins on by default");
        assert!(s.model.is_none());
        assert!(s.validate(0).is_ok());
    }

    #[test]
    fn keys_are_camel_case_for_the_frontend() {
        let v = serde_json::to_value(AssistSettings::default()).unwrap();
        for key in ["model", "aboutMe", "knowledge", "instructions", "cardKinds", "research", "builtin"] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
        let ts = include_str!("../../../src/lib/ai-assist-settings.ts");
        for key in ["aboutMe", "cardKinds", "allowedTools", "transport", "deepgramOverview", "deepgramDocs"] {
            assert!(ts.contains(key), "frontend settings type lacks {key}");
        }
    }

    #[test]
    fn validation_catches_what_the_ui_should_not_save() {
        let mut s = AssistSettings::default();
        s.card_kinds = CardKinds { answer: false, ask: false, fact: false, todo: false };
        assert!(s.validate(0).is_err());

        let mut s = AssistSettings::default();
        s.model = Some(ModelOverride { provider: "claude".into(), model: "".into() });
        assert!(s.validate(0).is_err());
        s.model = Some(ModelOverride { provider: "nope".into(), model: "m".into() });
        assert!(s.validate(0).is_err());
        s.model = Some(ModelOverride { provider: "claude".into(), model: "claude-haiku-5-5".into() });
        assert!(s.validate(0).is_ok());

        let mut s = AssistSettings::default();
        s.builtin.deepgram_overview = false;
        s.knowledge.push(KnowledgeItem { id: "a".into(), name: "big".into(), text: "x".repeat(MAX_CONTEXT_CHARS + 1), enabled: true });
        assert!(s.validate(0).is_err());
        s.knowledge[0].enabled = false;
        assert!(s.validate(0).is_ok(), "disabled knowledge does not count");
        s.knowledge[0].enabled = true;
        s.knowledge[0].text = "x".repeat(MAX_CONTEXT_CHARS - 10);
        assert!(s.validate(10).is_ok());
        assert!(s.validate(11).is_err(), "the brief shares the limit");

        let server = |name: &str, transport, command: &str, url: &str| McpServer {
            id: name.into(),
            name: name.into(),
            enabled: true,
            transport,
            command: command.into(),
            args: vec![],
            env: Default::default(),
            url: url.into(),
            headers: Default::default(),
            allowed_tools: vec![],
        };
        let mut s = AssistSettings::default();
        s.research.servers.push(server("a", McpTransport::Stdio, "", ""));
        assert!(s.validate(0).is_err(), "stdio without a command");
        s.research.servers[0].command = "/bin/true".into();
        assert!(s.validate(0).is_ok());
        s.research.servers.push(server("A", McpTransport::Http, "", "https://x"));
        assert!(s.validate(0).is_err(), "duplicate name");
        for reserved in ["deepgram DOCS", "pipecat docs", "Daily Docs"] {
            s.research.servers[1].name = reserved.into();
            assert!(s.validate(0).is_err(), "{reserved}: a built-in server's name is reserved");
        }
        s.research.servers[1].name = "b".into();
        s.research.servers[1].url = "ftp://x".into();
        assert!(s.validate(0).is_err(), "bad URL");
    }

    #[test]
    fn the_builtin_overview_counts_toward_the_context_limit() {
        let mut s = AssistSettings::default();
        let overview = DEEPGRAM_PUBLIC.chars().count();
        assert_eq!(s.context_chars(0), overview);
        s.knowledge.push(KnowledgeItem { id: "a".into(), name: "n".into(), text: "x".repeat(MAX_CONTEXT_CHARS - overview), enabled: true });
        assert!(s.validate(0).is_ok());
        assert_eq!(s.context_chars(0), MAX_CONTEXT_CHARS, "at the limit with the overview in");
        assert!(s.validate(1).is_ok(), "one more brief character: the overview steps out, the save passes");
        assert_eq!(s.context_chars(1), MAX_CONTEXT_CHARS - overview + 1);
        assert!(s.validate(overview + 1).is_err(), "own context and brief alone over the limit");
    }

    #[test]
    fn settings_saved_before_the_builtins_get_research_on() {
        let old = serde_json::json!({ "research": { "enabled": false, "servers": [] }, "aboutMe": "x" });
        let s = AssistSettings::from_stored(old).unwrap();
        assert!(s.research.enabled && s.builtin.deepgram_docs);
        let new = serde_json::json!({ "research": { "enabled": false, "servers": [] }, "builtin": { "deepgramDocs": true } });
        assert!(!AssistSettings::from_stored(new).unwrap().research.enabled, "a choice made with the built-ins present stands");
        let with_server = serde_json::json!({ "research": { "enabled": false, "servers": [{
            "id": "d", "name": "Mine", "transport": "stdio", "command": "x" }] } });
        assert!(!AssistSettings::from_stored(with_server).unwrap().research.enabled, "an opt-out with servers stands");
    }

    #[test]
    fn settings_saved_before_pipecat_daily_docs_follow_the_deepgram_docs_choice() {
        let off = serde_json::json!({ "builtin": { "deepgramOverview": true, "deepgramDocs": false } });
        assert!(!AssistSettings::from_stored(off).unwrap().builtin.pipecat_daily_docs);
        let on = serde_json::json!({ "builtin": { "deepgramOverview": true, "deepgramDocs": true } });
        assert!(AssistSettings::from_stored(on).unwrap().builtin.pipecat_daily_docs);
        let explicit = serde_json::json!({ "builtin": { "deepgramDocs": false, "pipecatDailyDocs": true } });
        assert!(AssistSettings::from_stored(explicit).unwrap().builtin.pipecat_daily_docs);
    }

    #[test]
    fn an_old_full_context_leaves_the_overview_out_instead_of_going_over() {
        let mut s = AssistSettings::default();
        assert!(s.overview_included(0));
        s.knowledge.push(KnowledgeItem { id: "a".into(), name: "n".into(), text: "x".repeat(MAX_CONTEXT_CHARS - 10), enabled: true });
        assert!(!s.overview_included(0), "it would push the prompt past the limit");
        assert!(s.validate(0).is_ok(), "and the save still passes: the overview is left out, not counted");
    }

    #[test]
    fn the_brief_counts_and_can_push_the_overview_out() {
        let overview = DEEPGRAM_PUBLIC.chars().count();
        let mut s = AssistSettings::default();
        s.about_me = "x".repeat(MAX_CONTEXT_CHARS - overview - 100);
        assert!(s.overview_included(100));
        assert_eq!(s.context_chars(100), MAX_CONTEXT_CHARS);
        assert!(!s.overview_included(101), "one more brief character and it no longer fits");
        assert_eq!(s.context_chars(101), MAX_CONTEXT_CHARS - overview + 1);
    }

    #[test]
    fn active_servers_need_research_and_the_server_on() {
        let mut r = Research { enabled: false, ..Default::default() };
        r.servers.push(McpServer {
            id: "d".into(),
            name: "DeepHive".into(),
            enabled: true,
            transport: McpTransport::Stdio,
            command: "x".into(),
            args: vec![],
            env: Default::default(),
            url: String::new(),
            headers: Default::default(),
            allowed_tools: vec![],
        });
        assert!(r.active_servers().is_empty());
        r.enabled = true;
        assert_eq!(r.active_servers().len(), 1);
        r.servers[0].enabled = false;
        assert!(r.active_servers().is_empty());
    }
}
