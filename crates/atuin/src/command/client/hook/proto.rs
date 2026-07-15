//! Typed protocol for the hook events coding agents send to `atuin hook`.
//!
//! Claude Code and Codex invoke `atuin hook <agent>` for each tool use and
//! pass the event as JSON on stdin. Both agents share the same schema (Codex
//! mirrors Claude Code's hook format), so a single set of wire types serves
//! both. This module decodes that JSON into typed structs and reduces it to a
//! small [`HookEvent`] the rest of the hook command acts on, instead of
//! walking a bare `serde_json::Value`.
//!
//! Compatibility notes:
//! - Unknown fields (e.g. `session_id`, `cwd`, and everything in
//!   `tool_response` besides `exitCode`) are ignored, so new agent fields
//!   never break parsing.
//! - Unrecognized `hook_event_name` values decode to [`HookEventName::Other`]
//!   and are skipped rather than erroring.
//! - Every field an agent may omit is optional, matching the previous
//!   permissive parsing.

use eyre::Result;
use serde::{Deserialize, Serialize};

/// The tool name agents use for shell execution. Only these events are
/// recorded; every other tool (file writes, web fetches, ...) is skipped.
pub const BASH_TOOL_NAME: &str = "Bash";

/// A hook event exactly as an agent serializes it on stdin.
///
/// Field types mirror the agent JSON schema. Missing fields decode to `None`;
/// unknown fields are ignored.
#[derive(Debug, Deserialize)]
pub struct WireHookEvent {
    #[serde(default)]
    pub hook_event_name: Option<HookEventName>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub tool_input: Option<WireToolInput>,
    #[serde(default)]
    pub tool_response: Option<WireToolResponse>,
}

/// The lifecycle stage an event represents.
///
/// The wire values are `PascalCase` and match these variant names exactly.
/// Unrecognized values map to [`HookEventName::Other`] so future or
/// agent-specific events are skipped rather than rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum HookEventName {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    #[serde(other)]
    Other,
}

/// The `tool_input` object: what the agent is about to run.
#[derive(Debug, Deserialize)]
pub struct WireToolInput {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// The `tool_response` object: how the command finished. Only the exit code is
/// consumed; all other fields are ignored.
#[derive(Debug, Deserialize)]
pub struct WireToolResponse {
    #[serde(rename = "exitCode", default)]
    pub exit_code: Option<i64>,
}

/// One entry in an agent's hook array: a matcher plus the command hooks to run
/// when it matches. This is the shape Atuin writes into (and looks for in) the
/// agent config file (`~/.claude/settings.json`, `~/.codex/hooks.json`).
///
/// Deserialization is permissive (unknown keys ignored) because the array also
/// holds entries other tools installed, which we must read past without error.
#[derive(Debug, Serialize, Deserialize)]
pub struct HookMatcher {
    pub matcher: String,
    pub hooks: Vec<HookCommand>,
}

/// A single command hook. `kind` serializes as the `"type"` field and is always
/// `"command"` for the hooks Atuin installs.
#[derive(Debug, Serialize, Deserialize)]
pub struct HookCommand {
    #[serde(rename = "type")]
    pub kind: String,
    pub command: String,
}

impl HookCommand {
    /// Build a `"command"`-type hook that runs `command`.
    pub fn command_hook(command: impl Into<String>) -> Self {
        Self {
            kind: "command".to_string(),
            command: command.into(),
        }
    }
}

/// The reduced event the hook command acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvent {
    /// A Bash command is about to run; open a history entry.
    Start {
        command: String,
        intent: Option<String>,
        tool_use_id: String,
    },
    /// A Bash command finished; close the matching history entry.
    End { tool_use_id: String, exit: i64 },
    /// Nothing to record (non-Bash tool, missing id, empty command, or an
    /// event we don't track).
    Skip,
}

impl WireHookEvent {
    /// Reduce a decoded wire event to a [`HookEvent`]. Infallible: anything we
    /// do not act on becomes [`HookEvent::Skip`].
    pub fn into_event(self) -> HookEvent {
        if self.tool_name.as_deref() != Some(BASH_TOOL_NAME) {
            return HookEvent::Skip;
        }

        let tool_use_id = match self.tool_use_id {
            Some(id) if !id.is_empty() => id,
            _ => return HookEvent::Skip,
        };

        match self.hook_event_name {
            Some(HookEventName::PreToolUse) => {
                let (command, intent) = match self.tool_input {
                    Some(input) => (input.command.unwrap_or_default(), input.description),
                    None => (String::new(), None),
                };

                if command.is_empty() {
                    return HookEvent::Skip;
                }

                HookEvent::Start {
                    command,
                    intent,
                    tool_use_id,
                }
            }
            Some(HookEventName::PostToolUse) => {
                let exit = self
                    .tool_response
                    .and_then(|response| response.exit_code)
                    .unwrap_or(0);
                HookEvent::End { tool_use_id, exit }
            }
            Some(HookEventName::PostToolUseFailure) => HookEvent::End {
                tool_use_id,
                exit: 1,
            },
            Some(HookEventName::Other) | None => HookEvent::Skip,
        }
    }
}

/// Parse a raw hook payload (the JSON an agent writes to stdin) into a
/// [`HookEvent`]. Errors only when the input is not valid JSON.
pub fn parse_hook_stdin(input: &str) -> Result<HookEvent> {
    let event: WireHookEvent = serde_json::from_str(input)?;
    Ok(event.into_event())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_tool_use_becomes_start() {
        let input = r#"{
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hello", "description": "Test greeting"},
            "tool_use_id": "toolu_abc123",
            "session_id": "sess1",
            "cwd": "/tmp"
        }"#;

        assert_eq!(
            parse_hook_stdin(input).unwrap(),
            HookEvent::Start {
                command: "echo hello".to_string(),
                intent: Some("Test greeting".to_string()),
                tool_use_id: "toolu_abc123".to_string(),
            }
        );
    }

    #[test]
    fn post_tool_use_becomes_end_with_exit_code() {
        let input = r#"{
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hello"},
            "tool_response": {"exitCode": 3, "stdout": "hello\n"},
            "tool_use_id": "toolu_abc123"
        }"#;

        assert_eq!(
            parse_hook_stdin(input).unwrap(),
            HookEvent::End {
                tool_use_id: "toolu_abc123".to_string(),
                exit: 3,
            }
        );
    }

    #[test]
    fn post_tool_use_without_exit_code_defaults_to_zero() {
        let input = r#"{
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_response": {},
            "tool_use_id": "toolu_abc123"
        }"#;

        assert_eq!(
            parse_hook_stdin(input).unwrap(),
            HookEvent::End {
                tool_use_id: "toolu_abc123".to_string(),
                exit: 0,
            }
        );
    }

    #[test]
    fn failure_event_forces_exit_one_and_ignores_response() {
        let input = r#"{
            "hook_event_name": "PostToolUseFailure",
            "tool_name": "Bash",
            "tool_input": {"command": "false"},
            "tool_response": {"exitCode": 0},
            "tool_use_id": "toolu_abc123"
        }"#;

        assert_eq!(
            parse_hook_stdin(input).unwrap(),
            HookEvent::End {
                tool_use_id: "toolu_abc123".to_string(),
                exit: 1,
            }
        );
    }

    #[test]
    fn non_bash_tool_is_skipped() {
        let input = r#"{
            "hook_event_name": "PreToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "/tmp/test.txt", "content": "hello"},
            "tool_use_id": "toolu_abc123"
        }"#;

        assert_eq!(parse_hook_stdin(input).unwrap(), HookEvent::Skip);
    }

    #[test]
    fn missing_tool_use_id_is_skipped() {
        let input = r#"{
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hi"}
        }"#;

        assert_eq!(parse_hook_stdin(input).unwrap(), HookEvent::Skip);
    }

    #[test]
    fn empty_tool_use_id_is_skipped() {
        let input = r#"{
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hi"},
            "tool_use_id": ""
        }"#;

        assert_eq!(parse_hook_stdin(input).unwrap(), HookEvent::Skip);
    }

    #[test]
    fn empty_command_is_skipped() {
        let input = r#"{
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": ""},
            "tool_use_id": "toolu_abc123"
        }"#;

        assert_eq!(parse_hook_stdin(input).unwrap(), HookEvent::Skip);
    }

    #[test]
    fn pre_tool_use_without_description_has_no_intent() {
        let input = r#"{
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
            "tool_use_id": "toolu_abc123"
        }"#;

        assert_eq!(
            parse_hook_stdin(input).unwrap(),
            HookEvent::Start {
                command: "ls".to_string(),
                intent: None,
                tool_use_id: "toolu_abc123".to_string(),
            }
        );
    }

    #[test]
    fn unknown_event_name_is_skipped() {
        let input = r#"{
            "hook_event_name": "SomeFutureEvent",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
            "tool_use_id": "toolu_abc123"
        }"#;

        assert_eq!(parse_hook_stdin(input).unwrap(), HookEvent::Skip);
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(parse_hook_stdin("not json").is_err());
    }

    #[test]
    fn hook_matcher_serializes_to_agent_schema() {
        let entry = HookMatcher {
            matcher: "Bash".to_string(),
            hooks: vec![HookCommand::command_hook("atuin hook claude-code")],
        };

        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            serde_json::json!({
                "matcher": "Bash",
                "hooks": [{"type": "command", "command": "atuin hook claude-code"}]
            })
        );
    }

    #[test]
    fn hook_matcher_roundtrips_and_exposes_command() {
        let value = serde_json::json!({
            "matcher": "Bash",
            "hooks": [{"type": "command", "command": "atuin hook claude-code"}]
        });

        let entry: HookMatcher = serde_json::from_value(value).unwrap();
        assert!(
            entry
                .hooks
                .iter()
                .any(|hook| hook.command == "atuin hook claude-code")
        );
    }

    #[test]
    fn hook_matcher_tolerates_foreign_fields() {
        // Other tools add entries with extra keys; deserializing our view of
        // them must ignore those keys rather than fail.
        let value = serde_json::json!({
            "matcher": "Bash",
            "hooks": [{"type": "command", "command": "some-other-tool", "timeout": 5}],
            "comment": "installed by another tool"
        });

        let entry: HookMatcher = serde_json::from_value(value).unwrap();
        assert_eq!(entry.hooks[0].command, "some-other-tool");
    }
}
