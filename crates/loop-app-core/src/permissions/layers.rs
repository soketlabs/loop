//! Policy: Capability settings layered from Managed Policy down to the
//! Session, where each layer can only tighten the ones above it (ADR 0004).
//!
//! The permission mode's preset says what a call may do by default; the
//! Policy is a ceiling on top. A call gets the stricter of the two, so an
//! allow rule can lift the mode's "ask" but never a layer's. A layer may
//! Lock a Capability: every lower layer's setting for it is then ignored,
//! even one that would tighten it.

use std::collections::{BTreeMap, BTreeSet};

use std::collections::BTreeMap as Rules;

use super::capability::{Capability, Setting, State};
use super::policy::{available, decide, Call, Decision};
use super::rules::ToolPermission;
use super::PermissionMode;

/// Where a Policy layer comes from, top first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LayerSource {
    /// Set on the device by the Org, which the user can't change.
    Managed,
    /// The Org's own layer.
    Org,
    /// A Group inside the Org.
    Group,
    /// The user's settings.
    User,
    /// A trusted project's settings.
    Project,
    /// An Agent Definition.
    AgentDefinition,
    /// One Session, such as a Subagent under its parent.
    Session,
}

impl LayerSource {
    /// The name in notices and refusals.
    pub fn label(self) -> &'static str {
        match self {
            Self::Managed => "Managed Policy",
            Self::Org => "Org Policy",
            Self::Group => "Group Policy",
            Self::User => "user settings",
            Self::Project => "project settings",
            Self::AgentDefinition => "the Agent Definition",
            Self::Session => "this Session's Policy",
        }
    }
}

/// One Policy layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    /// Where it comes from.
    pub source: LayerSource,
    /// Its Capability settings; a Capability it leaves out is unchanged.
    pub capabilities: BTreeMap<Capability, Setting>,
    /// Capabilities no lower layer may change.
    pub locks: BTreeSet<Capability>,
}

impl Layer {
    /// An empty layer.
    pub fn new(source: LayerSource) -> Self {
        Self {
            source,
            capabilities: BTreeMap::new(),
            locks: BTreeSet::new(),
        }
    }

    /// Set `capability`.
    pub fn set(mut self, capability: Capability, setting: Setting) -> Self {
        self.capabilities.insert(capability, setting);
        self
    }

    /// Lock `capability` for every lower layer.
    pub fn lock(mut self, capability: Capability) -> Self {
        self.locks.insert(capability);
        self
    }
}

/// The strictest state the Policy allows for a call, and the layer that
/// set it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ceiling {
    /// The state.
    pub state: State,
    /// The layer that set it.
    pub source: LayerSource,
}

/// The layers in force for a Session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    layers: Vec<Layer>,
}

impl Policy {
    /// A Policy from `layers` in any order. The notices name every setting a
    /// Lock above it makes Loop ignore.
    pub fn new(mut layers: Vec<Layer>) -> (Self, Vec<String>) {
        layers.sort_by_key(|layer| layer.source);
        let mut notices = Vec::new();
        for (i, layer) in layers.iter().enumerate() {
            for capability in layer.capabilities.keys() {
                if let Some(above) = layers[..i].iter().find(|l| l.locks.contains(capability)) {
                    notices.push(format!(
                        "{} locks {}; the setting in {} is ignored",
                        above.source.label(),
                        capability.label(),
                        layer.source.label()
                    ));
                }
            }
        }
        (Self { layers }, notices)
    }

    /// The settings that apply to `capability`, top first: down to and
    /// including the highest layer that locks it.
    fn settings(&self, capability: &Capability) -> Vec<(LayerSource, &Setting)> {
        let end = self
            .layers
            .iter()
            .position(|l| l.locks.contains(capability))
            .map_or(self.layers.len(), |i| i + 1);
        self.layers[..end]
            .iter()
            .filter_map(|l| l.capabilities.get(capability).map(|s| (l.source, s)))
            .collect()
    }

    /// The strictest state any layer gives `call`; `None` when no layer
    /// sets its Capability.
    pub fn ceiling(&self, call: &Call) -> Option<Ceiling> {
        let capability = Capability::of(call);
        self.settings(&capability)
            .into_iter()
            .map(|(source, setting)| Ceiling {
                state: setting.state_for(call),
                source,
            })
            .min_by_key(|ceiling| ceiling.state)
    }

    /// Whether some call of `call`'s tool could be anything but off.
    pub fn may_allow(&self, call: &Call) -> bool {
        let capability = Capability::of(call);
        self.settings(&capability)
            .into_iter()
            .all(|(_, setting)| setting.may_allow(call))
    }

    /// The decision for `call`: the mode's ([`decide`]), capped by the
    /// layers. Off in any layer refuses the call; "ask" in any layer asks
    /// even where the mode or an allow rule would not.
    pub fn decide(
        &self,
        mode: PermissionMode,
        rules: &Rules<String, ToolPermission>,
        call: &Call,
    ) -> Decision {
        let decision = decide(mode, rules, call);
        match (self.ceiling(call), decision) {
            (_, Decision::Deny(reason)) => Decision::Deny(reason),
            (
                Some(Ceiling {
                    state: State::Off,
                    source,
                }),
                _,
            ) => Decision::Deny(format!(
                "`{}` is not allowed: {} turns off {}",
                call.tool,
                source.label(),
                Capability::of(call).label()
            )),
            (
                Some(Ceiling {
                    state: State::Ask, ..
                }),
                Decision::Allow,
            ) => Decision::Ask,
            (_, decision) => decision,
        }
    }

    /// Whether a layer says to ask about `call`, so that not even a hook may
    /// run it without asking.
    pub fn requires_ask(&self, call: &Call) -> bool {
        self.ceiling(call)
            .is_some_and(|ceiling| ceiling.state == State::Ask)
    }

    /// Whether `call`'s tool stays in the toolset in `mode` under this
    /// Policy ([`available`]).
    pub fn available(&self, mode: PermissionMode, call: &Call) -> bool {
        available(mode, call) && self.may_allow(call)
    }

    /// A Policy with `layer` added below the others, for a Subagent: it can
    /// only tighten what its parent has.
    pub fn with(&self, layer: Layer) -> (Self, Vec<String>) {
        let mut layers = self.layers.clone();
        layers.push(layer);
        Self::new(layers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::capability::Scope;
    use serde_json::json;
    use Capability::*;
    use LayerSource::*;
    use State::{Ask, Off, On};

    fn bash(command: &str) -> Call {
        Call::new("bash", json!({ "command": command }), "/ws")
    }

    fn state(policy: &Policy, call: &Call) -> Option<State> {
        policy.ceiling(call).map(|c| c.state)
    }

    #[test]
    fn the_strictest_layer_wins() {
        let (policy, notices) = Policy::new(vec![
            Layer::new(User).set(Shell, Setting::always(On)),
            Layer::new(Org).set(Shell, Setting::always(Ask)),
        ]);
        assert!(notices.is_empty());
        let ceiling = policy.ceiling(&bash("ls")).unwrap();
        assert_eq!(ceiling.state, Ask, "a lower layer can't loosen Org's ask");
        assert_eq!(ceiling.source, Org);

        let (policy, _) = Policy::new(vec![
            Layer::new(Org).set(Shell, Setting::always(Ask)),
            Layer::new(Session).set(Shell, Setting::always(Off)),
        ]);
        let ceiling = policy.ceiling(&bash("ls")).unwrap();
        assert_eq!((ceiling.state, ceiling.source), (Off, Session));
    }

    #[test]
    fn a_layer_only_covers_the_capabilities_it_sets() {
        let (policy, _) = Policy::new(vec![Layer::new(Org).set(Shell, Setting::always(Off))]);
        assert_eq!(state(&policy, &bash("ls")), Some(Off));
        assert_eq!(state(&policy, &Call::new("read", json!({}), "/ws")), None);
        assert_eq!(state(&Policy::default(), &bash("ls")), None);
    }

    #[test]
    fn scopes_apply_per_call() {
        let (policy, _) = Policy::new(vec![
            Layer::new(Org).set(Shell, Setting::within(Off, Scope::ReadOnlyCommands, On))
        ]);
        assert_eq!(state(&policy, &bash("git status")), Some(On));
        assert_eq!(state(&policy, &bash("git push")), Some(Off));
        assert!(policy.may_allow(&Call::new("bash", serde_json::Value::Null, "/ws")));
    }

    #[test]
    fn a_lock_stops_lower_layers_changing_a_capability() {
        let (policy, notices) = Policy::new(vec![
            Layer::new(Org).set(Shell, Setting::always(On)).lock(Shell),
            Layer::new(User).set(Shell, Setting::always(Off)),
            Layer::new(User).set(WebFetch, Setting::always(Off)),
        ]);
        assert_eq!(
            state(&policy, &bash("ls")),
            Some(On),
            "locked on: a lower layer can't even tighten it"
        );
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].contains("Org Policy locks the shell"),
            "{notices:?}"
        );
        assert_eq!(
            state(&policy, &Call::new("web_fetch", json!({}), "/ws")),
            Some(Off),
            "a lock covers only its Capability"
        );

        let (policy, notices) = Policy::new(vec![
            Layer::new(Managed)
                .set(WriteFiles, Setting::always(Off))
                .lock(WriteFiles),
            Layer::new(Org).set(WriteFiles, Setting::always(On)),
        ]);
        let write = Call::new("write", json!({"path": "a"}), "/ws");
        assert_eq!(state(&policy, &write), Some(Off), "locked off");
        assert!(!policy.may_allow(&write));
        assert!(notices[0].contains("Managed Policy locks writing files"));
    }

    #[test]
    fn a_lock_without_a_setting_freezes_the_capability_as_it_is_above() {
        let (policy, _) = Policy::new(vec![
            Layer::new(Org).lock(Mcp("github".into())),
            Layer::new(User).set(Mcp("github".into()), Setting::always(Off)),
        ]);
        let call = Call::new("mcp__github__create_issue", json!({}), "/ws");
        assert_eq!(state(&policy, &call), None);
    }

    #[test]
    fn layers_cap_the_modes_decision() {
        let defaults = crate::tool_approval::permissions_from_settings(&Default::default());
        let allow_tests = crate::tool_approval::permissions_from_settings(
            &[("bash(cargo test*)".to_owned(), "allow".to_owned())].into(),
        );
        let (policy, _) = Policy::new(vec![
            Layer::new(Org).set(Shell, Setting::always(Ask)),
            Layer::new(User).set(WebFetch, Setting::always(Off)),
        ]);
        let fetch = Call::new("web_fetch", json!({"url": "https://a.example"}), "/ws");
        let read = Call::new("read", json!({"path": "a"}), "/ws");
        let tests = bash("cargo test");
        use PermissionMode::{FullAccess, ReadOnly};
        let rows: &[(PermissionMode, &Rules<String, ToolPermission>, &Call, &str)] = &[
            // Off in a layer refuses in every mode, full-access included.
            (FullAccess, &defaults, &fetch, "deny"),
            (ReadOnly, &defaults, &fetch, "deny"),
            // Ask in a layer asks where the mode or an allow rule would not.
            (FullAccess, &defaults, &tests, "ask"),
            (PermissionMode::Ask, &allow_tests, &tests, "ask"),
            // The mode still applies where no layer sets the Capability,
            // and a layer can't loosen it.
            (FullAccess, &defaults, &read, "allow"),
            (ReadOnly, &defaults, &bash("git push"), "deny"),
        ];
        for (mode, rules, call, want) in rows {
            let got = policy.decide(*mode, rules, call);
            let ok = match *want {
                "allow" => got == Decision::Allow,
                "ask" => got == Decision::Ask,
                _ => matches!(got, Decision::Deny(_)),
            };
            assert!(ok, "{mode:?} {}: want {want}, got {got:?}", call.tool);
        }
        let Decision::Deny(reason) = policy.decide(FullAccess, &defaults, &fetch) else {
            unreachable!()
        };
        assert!(
            reason.contains("user settings turns off web fetch"),
            "{reason}"
        );
        assert!(policy.requires_ask(&tests) && !policy.requires_ask(&read));
        assert!(!policy.available(
            FullAccess,
            &Call::new("web_fetch", serde_json::Value::Null, "/ws")
        ));
        assert!(policy.available(
            FullAccess,
            &Call::new("bash", serde_json::Value::Null, "/ws")
        ));
    }

    #[test]
    fn a_subagent_never_gets_more_than_its_parent() {
        let (parent, _) = Policy::new(vec![Layer::new(User).set(Shell, Setting::always(Ask))]);
        let (child, _) = parent.with(Layer::new(Session).set(Shell, Setting::always(On)));
        assert_eq!(state(&child, &bash("ls")), Some(Ask));
        let (child, _) = parent.with(Layer::new(Session).set(Shell, Setting::always(Off)));
        assert_eq!(state(&child, &bash("ls")), Some(Off));
    }
}
