use std::io::Read;
use std::path::PathBuf;

use atuin_client::settings::Settings;
use atuin_common::utils::home_dir;
use clap::{Parser, Subcommand};
use eyre::{Result, bail};
use serde::Deserialize;
use serde_json::Value;

use super::history;

mod event;
mod proto;

use event::{HookEvent, parse_hook_stdin};
use proto::{HookCommand, HookMatcher};

const HOOK_EVENT_TYPES: &[&str] = &["PreToolUse", "PostToolUse", "PostToolUseFailure"];
const PI_EXTENSION_SOURCE: &str = include_str!("../../../contrib/pi/atuin.ts");

enum InstallKind {
    JsonHooks {
        config_path: &'static [&'static str],
        hook_command: &'static str,
        matcher: &'static str,
    },
    PiExtension {
        extension_path: &'static [&'static str],
    },
}

struct AgentSpec {
    aliases: &'static [&'static str],
    actor_name: &'static str,
    install_kind: InstallKind,
}

const CLAUDE_CODE: AgentSpec = AgentSpec {
    aliases: &["claude-code", "claude"],
    actor_name: "claude-code",
    install_kind: InstallKind::JsonHooks {
        config_path: &[".claude", "settings.json"],
        hook_command: "atuin hook claude-code",
        matcher: "Bash",
    },
};

const CODEX: AgentSpec = AgentSpec {
    aliases: &["codex"],
    actor_name: "codex",
    install_kind: InstallKind::JsonHooks {
        config_path: &[".codex", "hooks.json"],
        hook_command: "atuin hook codex",
        matcher: "^Bash$",
    },
};

const PI: AgentSpec = AgentSpec {
    aliases: &["pi"],
    actor_name: "pi",
    install_kind: InstallKind::PiExtension {
        extension_path: &[".pi", "agent", "extensions", "atuin.ts"],
    },
};

const AGENTS: &[&AgentSpec] = &[&CLAUDE_CODE, &CODEX, &PI];

struct Agent(&'static AgentSpec);

impl Agent {
    fn from_name(name: &str) -> Result<Self> {
        AGENTS
            .iter()
            .copied()
            .find(|spec| spec.aliases.contains(&name))
            .map(Self)
            .ok_or_else(|| {
                eyre::eyre!("unknown agent: {name}. Supported agents: claude-code, codex, pi")
            })
    }

    fn actor_name(&self) -> &'static str {
        self.0.actor_name
    }

    fn path(path: &'static [&'static str]) -> PathBuf {
        path.iter()
            .fold(home_dir(), |path, segment| path.join(segment))
    }

    fn install_kind(&self) -> &InstallKind {
        &self.0.install_kind
    }
}

#[derive(Subcommand, Debug)]
enum Action {
    /// Install hooks for an AI agent to capture commands in atuin history
    Install {
        /// Agent to install hooks for (e.g., "claude-code")
        #[arg(value_name = "AGENT")]
        agent: String,
    },
}

#[derive(Parser, Debug)]
#[command(infer_subcommands = true, args_conflicts_with_subcommands = true)]
pub struct Cmd {
    #[command(subcommand)]
    action: Option<Action>,

    /// Which agent's hook format to parse (e.g., "claude-code")
    #[arg(value_name = "AGENT", hide = true)]
    agent: Option<String>,
}

impl Cmd {
    pub async fn run(self, settings: &Settings) -> Result<()> {
        match (self.action, self.agent) {
            (Some(Action::Install { agent }), None) => install(&agent),
            (None, Some(agent)) => handle(&agent, settings).await,
            (None, None) => bail!("expected `atuin hook <agent>` or `atuin hook install <agent>`"),
            (Some(_), Some(_)) => bail!("hook action cannot be combined with a positional agent"),
        }
    }
}

fn id_file_path(tool_use_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("atuin-hook-{tool_use_id}"))
}

async fn handle(agent_name: &str, settings: &Settings) -> Result<()> {
    let agent = Agent::from_name(agent_name)?;

    if matches!(agent.install_kind(), InstallKind::PiExtension { .. }) {
        bail!("`atuin hook pi` is not supported. Use `atuin hook install pi` and reload pi.");
    }

    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;

    if input.trim().is_empty() {
        return Ok(());
    }

    match parse_hook_stdin(&input)? {
        HookEvent::Start {
            command,
            intent,
            tool_use_id,
        } => {
            if let Some(history_id) = history::start_history_entry(
                settings,
                &command,
                Some(agent.actor_name()),
                intent.as_deref(),
            )
            .await?
            {
                std::fs::write(id_file_path(&tool_use_id), &history_id)?;
            }
        }
        HookEvent::End { tool_use_id, exit } => {
            let id_path = id_file_path(&tool_use_id);

            if let Ok(history_id) = std::fs::read_to_string(&id_path) {
                let history_id = history_id.trim();
                if !history_id.is_empty() {
                    let _ = history::end_history_entry(settings, history_id, exit, None).await;
                }
                let _ = std::fs::remove_file(&id_path);
            }
        }
        HookEvent::Skip => {}
    }

    Ok(())
}

fn install(agent_name: &str) -> Result<()> {
    let agent = Agent::from_name(agent_name)?;

    match agent.install_kind() {
        InstallKind::JsonHooks {
            config_path,
            hook_command: _,
            matcher: _,
        } => {
            let config_path = Agent::path(config_path);

            if let Some(parent) = config_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let mut root: Value = if config_path.exists() {
                let content = std::fs::read_to_string(&config_path)?;
                serde_json::from_str(&content)?
            } else {
                Value::Object(serde_json::Map::new())
            };

            let hooks = root
                .as_object_mut()
                .ok_or_else(|| eyre::eyre!("config is not a JSON object"))?
                .entry("hooks")
                .or_insert_with(|| Value::Object(serde_json::Map::new()));

            add_hook_entries(hooks, &agent)?;

            let content = serde_json::to_string_pretty(&root)?;
            std::fs::write(&config_path, content)?;

            eprintln!(
                "\nAtuin hooks installed for {}. Config: {}",
                agent.actor_name(),
                config_path.display()
            );
        }
        InstallKind::PiExtension { extension_path } => {
            let extension_path = Agent::path(extension_path);

            if let Some(parent) = extension_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let already_installed = std::fs::read_to_string(&extension_path)
                .is_ok_and(|existing| existing == PI_EXTENSION_SOURCE);

            if already_installed {
                eprintln!("pi extension: already installed, skipping");
            } else {
                std::fs::write(&extension_path, PI_EXTENSION_SOURCE)?;
                eprintln!("pi extension: installed atuin extension");
            }

            eprintln!(
                "\nAtuin extension installed for {}. Extension: {}\nReload pi with `/reload` or restart pi.",
                agent.actor_name(),
                extension_path.display()
            );
        }
    }

    Ok(())
}

fn add_hook_entries(hooks: &mut Value, agent: &Agent) -> Result<()> {
    let InstallKind::JsonHooks {
        config_path: _,
        hook_command,
        matcher,
    } = agent.install_kind()
    else {
        bail!("agent does not use JSON hooks")
    };

    let (matcher, hook_command) = (*matcher, *hook_command);

    for event_type in HOOK_EVENT_TYPES {
        let event_hooks = hooks
            .as_object_mut()
            .ok_or_else(|| eyre::eyre!("hooks is not a JSON object"))?
            .entry(*event_type)
            .or_insert_with(|| Value::Array(Vec::new()));

        let arr = event_hooks
            .as_array_mut()
            .ok_or_else(|| eyre::eyre!("hooks.{event_type} is not an array"))?;

        let already_installed = arr.iter().any(|entry| {
            HookMatcher::deserialize(entry)
                .is_ok_and(|entry| entry.hooks.iter().any(|hook| hook.command == hook_command))
        });

        if already_installed {
            eprintln!("hooks.{event_type}: already installed, skipping");
            continue;
        }

        let entry = HookMatcher {
            matcher: matcher.to_string(),
            hooks: vec![HookCommand::command_hook(hook_command)],
        };
        arr.push(serde_json::to_value(entry)?);
        eprintln!("hooks.{event_type}: installed atuin hook");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Atuin,
        command::{AtuinCmd, client},
    };
    use clap::Parser;

    #[test]
    fn parse_hook_agent_command() {
        let cmd = Cmd::try_parse_from(["hook", "codex"]).unwrap();

        assert!(matches!(
            (cmd.action, cmd.agent.as_deref()),
            (None, Some("codex"))
        ));
    }

    #[test]
    fn parse_hook_install_command() {
        let cmd = Cmd::try_parse_from(["hook", "install", "codex"]).unwrap();

        match (cmd.action, cmd.agent) {
            (Some(Action::Install { agent }), None) => assert_eq!(agent, "codex"),
            other => panic!("unexpected parsed command: {other:?}"),
        }
    }

    #[test]
    fn parse_hook_install_pi_command() {
        let cmd = Cmd::try_parse_from(["hook", "install", "pi"]).unwrap();

        match (cmd.action, cmd.agent) {
            (Some(Action::Install { agent }), None) => assert_eq!(agent, "pi"),
            other => panic!("unexpected parsed command: {other:?}"),
        }
    }

    #[test]
    fn agent_from_name_supports_pi() {
        let agent = Agent::from_name("pi").unwrap();
        assert_eq!(agent.actor_name(), "pi");
        assert!(matches!(
            agent.install_kind(),
            InstallKind::PiExtension { .. }
        ));
    }

    #[test]
    fn parse_top_level_hook_command() {
        let cmd = Atuin::try_parse_from(["atuin", "hook", "codex"]).unwrap();

        assert!(matches!(
            cmd.atuin,
            AtuinCmd::Client(client::Cmd::Hook(Cmd { action: None, agent: Some(agent) }))
                if agent == "codex"
        ));
    }

    #[test]
    fn add_hook_entries_is_idempotent() {
        let agent = Agent::from_name("claude-code").unwrap();
        let mut hooks = serde_json::json!({});

        add_hook_entries(&mut hooks, &agent).unwrap();
        add_hook_entries(&mut hooks, &agent).unwrap();

        for event_type in HOOK_EVENT_TYPES {
            let arr = hooks[*event_type].as_array().unwrap();
            assert_eq!(arr.len(), 1, "duplicate entry added for {event_type}");
        }
    }

    #[test]
    fn add_hook_entries_detects_installed_despite_malformed_sibling() {
        let agent = Agent::from_name("claude-code").unwrap();
        // Atuin's own command hook sharing a matcher entry with a foreign hook
        // that lacks the "type" field, as a user or another tool might merge.
        // The old whole-entry deserialize would fail here and duplicate; the
        // per-hook scan must still detect the existing atuin hook.
        let mut hooks = serde_json::json!({
            "PreToolUse": [{
                "matcher": "Bash",
                "hooks": [
                    {"type": "command", "command": "atuin hook claude-code"},
                    {"command": "some-other-tool"}
                ]
            }]
        });

        add_hook_entries(&mut hooks, &agent).unwrap();

        let arr = hooks["PreToolUse"].as_array().unwrap();
        assert_eq!(
            arr.len(),
            1,
            "should detect the existing atuin hook and not duplicate it"
        );
    }
}
