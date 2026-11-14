//! Permission modes: what Loop lets a tool call do without asking.
//!
//! Each mode is a preset over Capabilities; [`mode`] has the table. Policy
//! layers ([`layers`]) can only tighten what the mode allows.
//!
//! A `deny` in `toolPermissions` applies in every mode. `read-only` is a
//! ceiling: nothing, not even a hook, lets a call that changes files or runs
//! commands through.
//!
//! `toolPermissions` keys may carry a pattern on the call's main argument:
//! `bash(git status*)`, `write(~/secrets/*)`, `web_fetch(https://docs.rs/*)`.
//! A pattern rule beats the plain tool rule; among matching pattern rules
//! `deny` beats `ask` beats `allow`. A bash command is split at `;`, `&&`,
//! `||`, `|` and `&`: an `allow` pattern applies only when every part
//! matches one, and a command with `$(…)` or backticks never matches an
//! `allow` pattern. A path pattern matches the path as written and the
//! file it names through symlinks and `..`: `deny` and `ask` when any form
//! matches, `allow` only when all do. A pattern `ask` still asks in
//! `accept-edits`.
//!
//! - [`capability`]: Capabilities, their states and scopes.
//! - [`layers`]: Policy layers that only tighten, with Locks.
//! - [`mode`]: the four modes, as presets over Capabilities.
//! - [`rules`]: `toolPermissions` patterns and bash command splitting.
//! - [`policy`]: [`decide`], a function of the mode, the rules and a
//!   [`Call`] the gate resolved first.
//! - [`tool_lock`]: the Runtime's tool lock as the top Policy layer.
//! - [`decision`]: the decision record of every gate decision, and the sink
//!   it goes to.
//! - [`paths`]: where path checks get their facts: the Session's Workspace
//!   env ([`GatePaths`]), never the Runtime host disk, for a gate decision.

pub mod capability;
pub mod decision;
pub mod layers;
pub mod mode;
pub mod paths;
pub mod policy;
pub mod rules;
pub mod tool_lock;

pub use capability::{Capability, Scope, Setting, State};
pub use layers::{Ceiling, Layer, LayerSource, Policy};
pub use mode::PermissionMode;
pub use paths::{GatePaths, LookupStats, PathFacts};
pub use policy::{
    available, decide, decide_why, inside_workspace, workspace_root, Call, Decision, Verdict,
};
pub use rules::{
    read_only_command, rule_and_key, rule_for, search_may_show, search_may_show_in,
    search_may_show_with, split_command, Rule, ToolPermission,
};
