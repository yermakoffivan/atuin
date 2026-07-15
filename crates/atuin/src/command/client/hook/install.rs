//! Typed schema for the hook registrations Atuin writes into an agent's config
//! file (`~/.claude/settings.json`, `~/.codex/hooks.json`).
//!
//! This is a different protocol from the stdin hook *events* in
//! [`super::proto`]: those are what an agent sends us per tool use, whereas
//! these model the config-file entries `atuin hook install` reads and writes.
//! Only the entry Atuin owns is typed — the surrounding config document stays a
//! `serde_json::Value`, since it is arbitrary user JSON we must round-trip.

use serde::{Deserialize, Serialize};

/// One entry in an agent's hook array: a matcher plus the command hooks to run
/// when it matches. This is the shape Atuin writes into (and looks for in) the
/// agent config file.
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

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
