//! Permission modes: what Loop lets a tool call do without asking.
//!
//! | Mode | read, search, web | write / edit | bash | MCP tools |
//! |---|---|---|---|---|
//! | `read-only` | allow | deny | deny | deny |
//! | `ask` (default) | per `toolPermissions` (web asks) | ask | ask | per `toolPermissions` |
//! | `accept-edits` | allow | allow in the workspace; ask outside it and for `.git/`, `.loop/`, `.env*` | per `toolPermissions` | per `toolPermissions` |
//! | `full-access` | allow | allow | allow | allow |
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
//! `allow` pattern. A pattern `ask` still asks in `accept-edits`.
//!
//! - [`capability`]: Capabilities, their states and scopes.
//! - [`mode`]: the four modes, as presets over Capabilities.
//! - [`rules`]: `toolPermissions` patterns and bash command splitting.
//! - [`policy`]: [`decide`], a pure function of the mode, the rules and a
//!   [`Call`] the gate resolved first.

pub mod capability;
pub mod mode;
pub mod policy;
pub mod rules;

pub use capability::{Capability, Scope, Setting, State};
pub use mode::PermissionMode;
pub use policy::{available, decide, inside_workspace, workspace_root, Call, Decision};
pub use rules::{read_only_command, rule_for, split_command, Rule, ToolPermission};
