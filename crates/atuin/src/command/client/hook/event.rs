//! The domain event the hook command acts on.
//!
//! [`proto`](super::proto) models the raw JSON coding agents send on stdin;
//! this module reduces a decoded [`WireHookEvent`] to the small [`HookEvent`]
//! verb that [`handle`](super::handle) switches on. The reduction is where the
//! protocol's *meaning* lives: only `Bash` tools are recorded, a correlatable
//! `tool_use_id` is required, and a `PostToolUseFailure` is normalized to exit
//! status 1.

use eyre::Result;
use serde_json::error::Category;

use super::proto::{BASH_TOOL_NAME, HookEventName, WireHookEvent};

/// An actionable event the hook command records. A payload that is nothing to
/// record reduces to `None` rather than a variant — see the [`From`] impl and
/// [`HookEvent::from_json_str`].
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
}

impl From<WireHookEvent> for Option<HookEvent> {
    /// Reduce a decoded wire event to a [`HookEvent`], or `None` when there is
    /// nothing to record: a non-Bash tool, a missing or empty `tool_use_id`, an
    /// empty command, or an event stage we don't track.
    fn from(wire: WireHookEvent) -> Self {
        if wire.tool_name.as_str() != BASH_TOOL_NAME {
            return None;
        }

        // Present but empty is as good as missing: a start could never be
        // matched to its end.
        if wire.tool_use_id.is_empty() {
            return None;
        }
        let tool_use_id = wire.tool_use_id;

        match wire.hook_event_name {
            HookEventName::PreToolUse => {
                let (command, intent) = match wire.tool_input {
                    Some(input) => (input.command.unwrap_or_default(), input.description),
                    None => (String::new(), None),
                };

                if command.is_empty() {
                    return None;
                }

                Some(HookEvent::Start {
                    command,
                    intent,
                    tool_use_id,
                })
            }
            HookEventName::PostToolUse => {
                let exit = wire
                    .tool_response
                    .and_then(|response| response.exit_code)
                    .unwrap_or(0);
                Some(HookEvent::End { tool_use_id, exit })
            }
            HookEventName::PostToolUseFailure => Some(HookEvent::End {
                tool_use_id,
                exit: 1,
            }),
            HookEventName::Other => None,
        }
    }
}

impl HookEvent {
    /// Parse a raw hook payload (the JSON an agent writes to stdin) into a
    /// [`HookEvent`], or `None` when there is nothing to record.
    ///
    /// Well-formed JSON that doesn't fit the hook-event schema — a missing
    /// required field, a wrong-typed field, a value that isn't even an object —
    /// is not an event we handle, so it yields `Ok(None)`. Only *malformed*
    /// JSON (a syntax error or truncated input) is surfaced as an error: an
    /// agent could never send that legitimately, so it signals a real fault
    /// worth seeing.
    pub fn from_json_str(input: &str) -> Result<Option<Self>> {
        match serde_json::from_str::<WireHookEvent>(input) {
            Ok(wire) => Ok(wire.into()),
            Err(err) if err.classify() == Category::Data => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rstest::rstest;
    use serde_json::json;

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
        Some(HookEvent::Start {
            command: "echo hello".into(),
            intent: Some("Test greeting".into()),
            tool_use_id: "toolu_abc123".into(),
        })
    )]
    // No description → Start with no intent.
    #[case::pre_tool_use_without_description(
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
            "tool_use_id": "toolu_abc123"
        }),
        Some(HookEvent::Start { command: "ls".into(), intent: None, tool_use_id: "toolu_abc123".into() })
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
        Some(HookEvent::End { tool_use_id: "toolu_abc123".into(), exit: 3 })
    )]
    // Missing exitCode defaults to 0.
    #[case::post_tool_use_without_exit_code_defaults_zero(
        json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_response": {},
            "tool_use_id": "toolu_abc123"
        }),
        Some(HookEvent::End { tool_use_id: "toolu_abc123".into(), exit: 0 })
    )]
    // A null exitCode also defaults to 0.
    #[case::null_exit_code_defaults_zero(
        json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_response": {"exitCode": null},
            "tool_use_id": "toolu_abc123"
        }),
        Some(HookEvent::End { tool_use_id: "toolu_abc123".into(), exit: 0 })
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
        Some(HookEvent::End { tool_use_id: "toolu_abc123".into(), exit: 1 })
    )]
    // Non-Bash tools are never recorded.
    #[case::non_bash_tool_skipped(
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "/tmp/test.txt", "content": "hello"},
            "tool_use_id": "toolu_abc123"
        }),
        None
    )]
    // A missing tool_use_id can't be correlated start↔end → skip.
    #[case::missing_tool_use_id_skipped(
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hi"}
        }),
        None
    )]
    // An empty tool_use_id is treated the same as missing.
    #[case::empty_tool_use_id_skipped(
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hi"},
            "tool_use_id": ""
        }),
        None
    )]
    // An empty command has nothing to record.
    #[case::empty_command_skipped(
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": ""},
            "tool_use_id": "toolu_abc123"
        }),
        None
    )]
    // No tool_input at all → empty command → skip.
    #[case::missing_tool_input_skipped(
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_use_id": "toolu_abc123"
        }),
        None
    )]
    // A null tool_input decodes to None → skip.
    #[case::null_tool_input_skipped(
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": null,
            "tool_use_id": "toolu_abc123"
        }),
        None
    )]
    // An event name we don't model is ignored.
    #[case::unknown_event_skipped(
        json!({
            "hook_event_name": "SomeFutureEvent",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
            "tool_use_id": "toolu_abc123"
        }),
        None
    )]
    // A missing event name is ignored.
    #[case::missing_event_skipped(
        json!({
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
            "tool_use_id": "toolu_abc123"
        }),
        None
    )]
    fn parses_agent_event(#[case] input: serde_json::Value, #[case] expected: Option<HookEvent>) {
        assert_eq!(HookEvent::from_json_str(&input.to_string()).unwrap(), expected);
    }

    /// Well-formed JSON that isn't a hook event we model is skipped, not an
    /// error — it decodes cleanly as JSON but doesn't fit the schema.
    #[rstest]
    #[case::json_but_not_an_object("42")]
    #[case::missing_required_fields(r#"{"tool_name": "Bash"}"#)]
    #[case::wrong_typed_tool_use_id(
        r#"{"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": 5, "tool_input": {"command": "ls"}}"#
    )]
    fn well_formed_non_events_are_skipped(#[case] input: &str) {
        assert_eq!(HookEvent::from_json_str(input).unwrap(), None);
    }

    /// Malformed JSON is a genuine fault an agent could never send by design,
    /// so it surfaces as an error rather than being silently dropped.
    #[rstest]
    #[case::not_json("not json")]
    #[case::truncated(r#"{"tool_name":"#)]
    fn malformed_json_is_an_error(#[case] input: &str) {
        assert!(HookEvent::from_json_str(input).is_err());
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
                HookEvent::from_json_str(&input.to_string()).unwrap(),
                Some(HookEvent::Start { command, intent: description, tool_use_id })
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
                HookEvent::from_json_str(&input.to_string()).unwrap(),
                Some(HookEvent::End { tool_use_id, exit })
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
                HookEvent::from_json_str(&input.to_string()).unwrap(),
                Some(HookEvent::End { tool_use_id, exit: 1 })
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

            prop_assert_eq!(HookEvent::from_json_str(&input.to_string()).unwrap(), None);
        }
    }
}
