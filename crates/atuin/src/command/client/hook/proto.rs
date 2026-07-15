//! Typed wire protocol for the hook events coding agents send to `atuin hook`.
//!
//! Claude Code and Codex invoke `atuin hook <agent>` for each tool use and
//! pass the event as JSON on stdin. Both agents share the same schema (Codex
//! mirrors Claude Code's hook format), so a single set of wire types serves
//! both. This module only *models* that JSON; reducing a decoded event to the
//! verb the command acts on lives in [`super::event`], and the separate schema
//! for the config-file entries `atuin hook install` writes lives in
//! [`super::install`].
//!
//! Compatibility notes:
//! - Unknown fields (e.g. `session_id`, `cwd`, and everything in
//!   `tool_response` besides `exitCode`) are ignored, so new agent fields
//!   never break parsing.
//! - Unrecognized `hook_event_name` values decode to [`HookEventName::Other`]
//!   and are skipped rather than erroring.
//! - The three fields every event must have to be actionable — the stage, the
//!   tool, and the correlation id — are required; a payload missing any of them
//!   fails to deserialize (and is then skipped). The stage-specific
//!   `tool_input` / `tool_response` payloads are optional.

use serde::Deserialize;

/// The tool name agents use for shell execution. Only these events are
/// recorded; every other tool (file writes, web fetches, ...) is skipped.
pub const BASH_TOOL_NAME: &str = "Bash";

/// A hook event exactly as an agent serializes it on stdin.
///
/// The three fields every event carries — the stage, the tool, and the
/// correlation id — are required, so a payload missing any of them fails to
/// deserialize and is skipped. The stage-specific payloads are optional.
/// Unknown fields are ignored.
#[derive(Debug, Deserialize)]
pub struct WireHookEvent {
    /// The lifecycle stage. An unrecognized value decodes to
    /// [`HookEventName::Other`]; a missing field fails deserialization.
    pub hook_event_name: HookEventName,
    /// The tool that ran; we only record `Bash`.
    pub tool_name: String,
    /// Correlates a command's start and end across two `atuin hook`
    /// invocations.
    pub tool_use_id: String,
    /// The command about to run. Present on `PreToolUse`; absent (and unread)
    /// on the completion events.
    #[serde(default)]
    pub tool_input: Option<WireToolInput>,
    /// How the command finished. Present on `PostToolUse`; absent elsewhere.
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
