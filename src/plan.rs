//! Terraform plan JSON parser.
//!
//! Supports `format_version` 1.0, 1.1, 1.2 (stable schema since Terraform 1.0, July 2021).
//! Schema reference: <https://developer.hashicorp.com/terraform/internals/json-format>

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use serde_json::Value;
use std::fmt;

const SUPPORTED_FORMAT_VERSIONS: &[&str] = &["1.0", "1.1", "1.2"];

#[derive(Debug, Clone, Deserialize)]
pub struct Plan {
    pub format_version: String,
    #[serde(default)]
    pub resource_changes: Vec<ResourceChange>,
    #[serde(default)]
    pub resource_drift: Vec<ResourceChange>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // name/provider_name/module_address parsed for future features.
pub struct ResourceChange {
    pub address: String,
    #[serde(rename = "type")]
    pub resource_type: String,
    pub change: Change,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub provider_name: Option<String>,
    #[serde(default)]
    pub module_address: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Change {
    pub actions: Vec<String>,
    #[serde(default)]
    pub before: Option<Value>,
    #[serde(default)]
    pub after: Option<Value>,
    #[serde(default)]
    pub after_unknown: Option<Value>,
    #[serde(default)]
    pub before_sensitive: Option<Value>,
    #[serde(default)]
    pub after_sensitive: Option<Value>,
}

/// Parsed plan footer counts, mirroring `terraform plan`'s own summary line.
/// Replace is counted as `add + destroy` (matching terraform's convention).
#[derive(Debug, Default, Clone, Copy)]
pub struct ChangeCounts {
    pub add: usize,
    pub change: usize,
    pub destroy: usize,
    pub read: usize,
    pub drift: usize,
}

impl ChangeCounts {
    pub fn footer(&self) -> String {
        let mut parts = vec![
            format!("{} to add", self.add),
            format!("{} to change", self.change),
            format!("{} to destroy", self.destroy),
        ];
        if self.read > 0 {
            parts.push(format!("{} to read", self.read));
        }
        if self.drift > 0 {
            parts.push(format!("{} drifted", self.drift));
        }
        parts.join(", ")
    }
}

/// High-level action derived from the `actions` array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    Create,
    Update,
    Delete,
    Replace,
    Read,
    NoOp,
}

impl Action {
    /// Maps the string array from terraform JSON to a single high-level action.
    /// - `["create"]` -> Create
    /// - `["update"]` -> Update
    /// - `["delete"]` -> Delete
    /// - `["delete", "create"]` / `["create", "delete"]` -> Replace
    /// - `["read"]` -> Read
    /// - `["no-op"]` -> NoOp
    pub fn from_actions(actions: &[String]) -> Result<Self> {
        let a: Vec<&str> = actions.iter().map(String::as_str).collect();
        match a.as_slice() {
            ["create"] => Ok(Action::Create),
            ["update"] => Ok(Action::Update),
            ["delete"] => Ok(Action::Delete),
            ["delete", "create"] | ["create", "delete"] => Ok(Action::Replace),
            ["read"] => Ok(Action::Read),
            ["no-op"] => Ok(Action::NoOp),
            other => Err(anyhow!("unrecognized action set: {other:?}")),
        }
    }

    /// Compact single-character glyph used in digests and tree labels.
    pub fn glyph(&self) -> &'static str {
        match self {
            Action::Create => "+",
            Action::Update => "~",
            Action::Delete => "-",
            Action::Replace => "-/+",
            Action::Read => ">",
            Action::NoOp => " ",
        }
    }

    #[allow(dead_code)]
    pub fn label(&self) -> &'static str {
        match self {
            Action::Create => "create",
            Action::Update => "update",
            Action::Delete => "delete",
            Action::Replace => "replace",
            Action::Read => "read",
            Action::NoOp => "no-op",
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.glyph())
    }
}

impl Plan {
    pub fn parse(raw: &str) -> Result<Self> {
        let plan: Plan = serde_json::from_str(raw)?;
        if !SUPPORTED_FORMAT_VERSIONS.contains(&plan.format_version.as_str()) {
            bail!(
                "unsupported plan format_version {:?} (supported: {})",
                plan.format_version,
                SUPPORTED_FORMAT_VERSIONS.join(", ")
            );
        }
        Ok(plan)
    }

    /// Breakdown matching `terraform plan`'s "Plan: X to add, Y to change,
    /// Z to destroy" footer. Counts only `resource_changes` entries with
    /// non-noop actions — no-ops and drift are reported separately.
    pub fn change_counts(&self) -> ChangeCounts {
        let mut c = ChangeCounts::default();
        for rc in &self.resource_changes {
            match Action::from_actions(&rc.change.actions) {
                Ok(Action::Create) => c.add += 1,
                Ok(Action::Update) => c.change += 1,
                Ok(Action::Delete) => c.destroy += 1,
                Ok(Action::Replace) => {
                    c.add += 1;
                    c.destroy += 1;
                }
                Ok(Action::Read) => c.read += 1,
                _ => {}
            }
        }
        c.drift = self.resource_drift.len();
        c
    }

    /// Summed count of pending plan actions. Mirrors terraform's footer
    /// semantics — drift is NOT included here since terraform treats it
    /// as informational, not a plan action.
    pub fn total_changes(&self) -> usize {
        let c = self.change_counts();
        c.add + c.change + c.destroy + c.read
    }

    /// True if every change is a no-op (what `terraform show -json` emits
    /// for a plan with no pending changes).
    #[allow(dead_code)]
    pub fn is_empty_plan(&self) -> bool {
        self.resource_drift.is_empty()
            && self.resource_changes.iter().all(|rc| {
                Action::from_actions(&rc.change.actions)
                    .map(|a| a == Action::NoOp)
                    .unwrap_or(false)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_plan() {
        let raw = r#"{"format_version":"1.2","resource_changes":[]}"#;
        let p = Plan::parse(raw).unwrap();
        assert_eq!(p.format_version, "1.2");
        assert!(p.is_empty_plan());
    }

    #[test]
    fn parse_no_op_is_empty_plan() {
        let raw = r#"{
            "format_version":"1.2",
            "resource_changes":[
                {"address":"null_resource.a","type":"null_resource","change":{"actions":["no-op"]}}
            ]
        }"#;
        let p = Plan::parse(raw).unwrap();
        assert!(p.is_empty_plan());
    }

    #[test]
    fn rejects_unsupported_version() {
        let raw = r#"{"format_version":"0.9"}"#;
        let err = Plan::parse(raw).unwrap_err();
        assert!(err.to_string().contains("unsupported plan format_version"));
    }

    #[test]
    fn actions_map() {
        assert_eq!(
            Action::from_actions(&["create".into()]).unwrap(),
            Action::Create
        );
        assert_eq!(
            Action::from_actions(&["delete".into(), "create".into()]).unwrap(),
            Action::Replace
        );
        assert_eq!(
            Action::from_actions(&["create".into(), "delete".into()]).unwrap(),
            Action::Replace
        );
        assert_eq!(
            Action::from_actions(&["no-op".into()]).unwrap(),
            Action::NoOp
        );
        assert!(Action::from_actions(&["garbage".into()]).is_err());
    }

    #[test]
    fn parses_replace_action_in_resource_change() {
        let raw = r#"{
            "format_version":"1.1",
            "resource_changes":[
                {"address":"aws_instance.web","type":"aws_instance","change":{"actions":["delete","create"]}}
            ]
        }"#;
        let p = Plan::parse(raw).unwrap();
        let action = Action::from_actions(&p.resource_changes[0].change.actions).unwrap();
        assert_eq!(action, Action::Replace);
    }

    #[test]
    fn parses_resource_drift() {
        let raw = r#"{
            "format_version":"1.2",
            "resource_changes":[],
            "resource_drift":[
                {"address":"aws_instance.web","type":"aws_instance","change":{"actions":["update"]}}
            ]
        }"#;
        let p = Plan::parse(raw).unwrap();
        assert_eq!(p.resource_drift.len(), 1);
        // Drift is NOT counted in total_changes (matches terraform footer).
        assert_eq!(p.total_changes(), 0);
        assert_eq!(p.change_counts().drift, 1);
    }
}
