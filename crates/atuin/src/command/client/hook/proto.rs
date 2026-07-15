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
/// Deserialization is deliberately **partial**: the array Atuin scans also holds
/// entries other tools installed, so unknown keys are ignored, and — via
/// [`deserialize_partial_hooks`] — any element of `hooks` that does not fit
/// [`HookCommand`] is dropped rather than failing the whole entry. This keeps
/// detection per-hook: a single malformed sibling hook can't hide the atuin hook
/// living beside it. Serialization is unaffected — Atuin only ever writes
/// well-formed entries.
#[derive(Debug, Serialize, Deserialize)]
pub struct HookMatcher {
    pub matcher: String,
    #[serde(deserialize_with = "deserialize_partial_hooks")]
    pub hooks: Vec<HookCommand>,
}

/// Deserialize a `hooks` array, keeping only the elements that decode as a
/// [`HookCommand`] and silently dropping the rest. Foreign tools may add hooks
/// in shapes we don't model; those must not abort reading the array.
fn deserialize_partial_hooks<'de, D>(deserializer: D) -> Result<Vec<HookCommand>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw
        .iter()
        .filter_map(|value| HookCommand::deserialize(value).ok())
        .collect())
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
    use proptest::prelude::*;
    use rstest::rstest;
    use serde_json::json;

    // ---- inbound: parse_hook_stdin -> HookEvent -------------------------

    /// Table of the exact wire-event → [`HookEvent`] mapping. Each row is its
    /// own case, so a failure names precisely which payload broke.
    #[rstest]
    // PreToolUse with a command and description → Start carrying the intent.
    #[case::pre_tool_use_with_intent(
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hello", "description": "Test greeting"},
            "tool_use_id": "toolu_abc123",
            "session_id": "sess1",
            "cwd": "/tmp"
        }),
        HookEvent::Start {
            command: "echo hello".into(),
            intent: Some("Test greeting".into()),
            tool_use_id: "toolu_abc123".into(),
        }
    )]
    // No description → Start with no intent.
    #[case::pre_tool_use_without_description(
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
            "tool_use_id": "toolu_abc123"
        }),
        HookEvent::Start { command: "ls".into(), intent: None, tool_use_id: "toolu_abc123".into() }
    )]
    // PostToolUse → End carrying the reported exit code.
    #[case::post_tool_use_uses_exit_code(
        json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hello"},
            "tool_response": {"exitCode": 3, "stdout": "hello\n"},
            "tool_use_id": "toolu_abc123"
        }),
        HookEvent::End { tool_use_id: "toolu_abc123".into(), exit: 3 }
    )]
    // Missing exitCode defaults to 0.
    #[case::post_tool_use_without_exit_code_defaults_zero(
        json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_response": {},
            "tool_use_id": "toolu_abc123"
        }),
        HookEvent::End { tool_use_id: "toolu_abc123".into(), exit: 0 }
    )]
    // A null exitCode also defaults to 0.
    #[case::null_exit_code_defaults_zero(
        json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_response": {"exitCode": null},
            "tool_use_id": "toolu_abc123"
        }),
        HookEvent::End { tool_use_id: "toolu_abc123".into(), exit: 0 }
    )]
    // PostToolUseFailure forces exit 1 and ignores tool_response entirely.
    #[case::failure_forces_exit_one_ignoring_response(
        json!({
            "hook_event_name": "PostToolUseFailure",
            "tool_name": "Bash",
            "tool_input": {"command": "false"},
            "tool_response": {"exitCode": 0},
            "tool_use_id": "toolu_abc123"
        }),
        HookEvent::End { tool_use_id: "toolu_abc123".into(), exit: 1 }
    )]
    // Non-Bash tools are never recorded.
    #[case::non_bash_tool_skipped(
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "/tmp/test.txt", "content": "hello"},
            "tool_use_id": "toolu_abc123"
        }),
        HookEvent::Skip
    )]
    // A missing tool_use_id can't be correlated start↔end → skip.
    #[case::missing_tool_use_id_skipped(
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hi"}
        }),
        HookEvent::Skip
    )]
    // An empty tool_use_id is treated the same as missing.
    #[case::empty_tool_use_id_skipped(
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hi"},
            "tool_use_id": ""
        }),
        HookEvent::Skip
    )]
    // An empty command has nothing to record.
    #[case::empty_command_skipped(
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": ""},
            "tool_use_id": "toolu_abc123"
        }),
        HookEvent::Skip
    )]
    // No tool_input at all → empty command → skip.
    #[case::missing_tool_input_skipped(
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_use_id": "toolu_abc123"
        }),
        HookEvent::Skip
    )]
    // A null tool_input decodes to None → skip.
    #[case::null_tool_input_skipped(
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": null,
            "tool_use_id": "toolu_abc123"
        }),
        HookEvent::Skip
    )]
    // An event name we don't model is ignored.
    #[case::unknown_event_skipped(
        json!({
            "hook_event_name": "SomeFutureEvent",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
            "tool_use_id": "toolu_abc123"
        }),
        HookEvent::Skip
    )]
    // A missing event name is ignored.
    #[case::missing_event_skipped(
        json!({
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
            "tool_use_id": "toolu_abc123"
        }),
        HookEvent::Skip
    )]
    fn parses_agent_event(#[case] input: serde_json::Value, #[case] expected: HookEvent) {
        assert_eq!(parse_hook_stdin(&input.to_string()).unwrap(), expected);
    }

    /// Payloads that aren't a hook-event object must error rather than parse.
    #[rstest]
    #[case::not_json("not json")]
    #[case::truncated(r#"{"tool_name":"#)]
    #[case::json_but_not_an_object("42")]
    fn rejects_non_object_payloads(#[case] input: &str) {
        assert!(parse_hook_stdin(input).is_err());
    }

    proptest! {
        /// Any Bash `PreToolUse` with a non-empty command becomes a `Start`
        /// carrying that command, the tool id, and the optional description as
        /// intent — regardless of the surrounding fields.
        #[test]
        fn bash_pre_tool_use_yields_start(
            command in r"[^\p{Cc}]+",
            tool_use_id in r"[^\p{Cc}]+",
            description in proptest::option::of(r"[^\p{Cc}]*"),
        ) {
            let mut tool_input = serde_json::Map::new();
            tool_input.insert("command".to_string(), json!(command));
            if let Some(intent) = &description {
                tool_input.insert("description".to_string(), json!(intent));
            }
            let input = json!({
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "tool_input": serde_json::Value::Object(tool_input),
                "tool_use_id": tool_use_id,
            });

            prop_assert_eq!(
                parse_hook_stdin(&input.to_string()).unwrap(),
                HookEvent::Start { command, intent: description, tool_use_id }
            );
        }

        /// Any Bash `PostToolUse` reports the exit code verbatim, for every i64.
        #[test]
        fn bash_post_tool_use_reports_exit_code(
            exit in any::<i64>(),
            tool_use_id in r"[^\p{Cc}]+",
        ) {
            let input = json!({
                "hook_event_name": "PostToolUse",
                "tool_name": "Bash",
                "tool_response": {"exitCode": exit},
                "tool_use_id": tool_use_id,
            });

            prop_assert_eq!(
                parse_hook_stdin(&input.to_string()).unwrap(),
                HookEvent::End { tool_use_id, exit }
            );
        }

        /// `PostToolUseFailure` always records exit 1, whatever the response
        /// claims.
        #[test]
        fn failure_event_always_exits_one(
            reported_exit in any::<i64>(),
            tool_use_id in r"[^\p{Cc}]+",
        ) {
            let input = json!({
                "hook_event_name": "PostToolUseFailure",
                "tool_name": "Bash",
                "tool_response": {"exitCode": reported_exit},
                "tool_use_id": tool_use_id,
            });

            prop_assert_eq!(
                parse_hook_stdin(&input.to_string()).unwrap(),
                HookEvent::End { tool_use_id, exit: 1 }
            );
        }

        /// Any tool other than Bash is skipped, whatever the event or fields.
        #[test]
        fn non_bash_tool_is_always_skipped(
            tool_name in r"[^\p{Cc}]+".prop_filter("must not be Bash", |s| s.as_str() != "Bash"),
            event in proptest::sample::select(vec![
                "PreToolUse", "PostToolUse", "PostToolUseFailure", "Frobnicate",
            ]),
            tool_use_id in r"[^\p{Cc}]+",
        ) {
            let input = json!({
                "hook_event_name": event,
                "tool_name": tool_name,
                "tool_input": {"command": "ls"},
                "tool_response": {"exitCode": 0},
                "tool_use_id": tool_use_id,
            });

            prop_assert_eq!(parse_hook_stdin(&input.to_string()).unwrap(), HookEvent::Skip);
        }
    }

    // ---- outbound: install-entry (de)serialization ----------------------

    /// The install entry Atuin writes must match the agents' expected schema
    /// exactly, so configs written by older versions keep being recognized.
    #[test]
    fn hook_matcher_serializes_to_agent_schema() {
        let entry = HookMatcher {
            matcher: "Bash".to_string(),
            hooks: vec![HookCommand::command_hook("atuin hook claude-code")],
        };

        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            json!({
                "matcher": "Bash",
                "hooks": [{"type": "command", "command": "atuin hook claude-code"}]
            })
        );
    }

    proptest! {
        /// `command_hook` always produces the `{"type":"command","command":..}`
        /// shape, for any command string.
        #[test]
        fn command_hook_always_uses_type_command(command in r"[^\p{Cc}]+") {
            prop_assert_eq!(
                serde_json::to_value(HookCommand::command_hook(&command)).unwrap(),
                json!({"type": "command", "command": command})
            );
        }

        /// A `HookMatcher` of arbitrary commands round-trips through JSON with
        /// its commands intact and in order.
        #[test]
        fn hook_matcher_round_trips_commands(
            commands in proptest::collection::vec(r"[^\p{Cc}]+", 0..8),
        ) {
            let entry = HookMatcher {
                matcher: "Bash".to_string(),
                hooks: commands.iter().map(HookCommand::command_hook).collect(),
            };

            let value = serde_json::to_value(&entry).unwrap();
            let restored: HookMatcher = serde_json::from_value(value).unwrap();
            let restored: Vec<String> = restored.hooks.into_iter().map(|hook| hook.command).collect();

            prop_assert_eq!(restored, commands);
        }

        /// Deserializing an entry keeps exactly the well-formed command hooks,
        /// in order — dropping any malformed sibling and ignoring unknown keys
        /// on both the hooks and the entry — no matter how a foreign tool
        /// interleaved its own entries.
        #[test]
        fn hook_matcher_keeps_only_well_formed_hooks(
            specs in proptest::collection::vec(
                prop_oneof![
                    r"[^\p{Cc}]+".prop_map(Some),
                    Just(None),
                ],
                0..10,
            ),
        ) {
            let hooks: Vec<serde_json::Value> = specs
                .iter()
                .map(|spec| {
                    spec.as_ref().map_or_else(
                        // A hook we don't model at all — must be dropped.
                        || json!({"comment": "a foreign hook we don't model"}),
                        // A well-formed command hook, with an extra key a foreign
                        // tool might add — which must be ignored, not rejected.
                        |command| json!({"type": "command", "command": command, "timeout": 5}),
                    )
                })
                .collect();
            let value = json!({
                "matcher": "Bash",
                "hooks": hooks,
                "installed_by": "another tool",
            });

            let entry: HookMatcher = serde_json::from_value(value).unwrap();

            let expected: Vec<&String> = specs.iter().flatten().collect();
            let got: Vec<&String> = entry.hooks.iter().map(|hook| &hook.command).collect();

            prop_assert_eq!(got, expected);
        }
    }
}
