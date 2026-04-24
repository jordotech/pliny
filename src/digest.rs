//! Pre-digest the plan to compact text for the LLM prompt.
//!
//! Strategy:
//! - For plans with < [`FULL_DIGEST_LIMIT`] resources: one line per resource change,
//!   with up to [`MAX_ATTRS_PER_RESOURCE`] changed/notable attributes.
//! - For bigger plans: summary-of-summaries — just group counts, no individual resources.
//!
//! Sensitive values (anything flagged via `before_sensitive`/`after_sensitive` or
//! `after_unknown`) are replaced with `<sensitive>` before the line is emitted.
//! Sensitive data MUST NEVER be sent to an external LLM.

use crate::groups::Group;
use crate::plan::{Action, Plan};
use serde_json::{Map, Value};

pub const FULL_DIGEST_LIMIT: usize = 500;
pub const MAX_ATTRS_PER_RESOURCE: usize = 5;
pub const MAX_DIGEST_TOKENS: usize = 4000;
const SENSITIVE_PLACEHOLDER: &str = "<sensitive>";

/// Produce pre-digest text from a plan + its groups.
pub fn digest_plan(plan: &Plan, groups: &[Group]) -> String {
    let counts = plan.change_counts();
    let total = plan.total_changes();
    let mut out = String::new();

    out.push_str(&format!(
        "Plan: {}. {} group(s) by type/action.\n\n",
        counts.footer(),
        groups.len()
    ));

    if total >= FULL_DIGEST_LIMIT {
        out.push_str("# Group summary (detailed per-resource digest omitted due to size)\n");
        for g in groups {
            out.push_str(&format!(
                "{} {} ({})\n",
                g.action.glyph(),
                g.resource_type,
                g.count()
            ));
        }
        return out;
    }

    for g in groups {
        out.push_str(&format!(
            "\n## {} {} ({})\n",
            g.action.glyph(),
            g.resource_type,
            g.count()
        ));
        for rc in &g.changes {
            let attrs = digest_attrs(&rc.change.actions, &rc.change);
            let action = Action::from_actions(&rc.change.actions).unwrap_or(Action::NoOp);
            out.push_str(&format!(
                "{} {} {}\n",
                action.glyph(),
                rc.address,
                attrs
            ));
        }
    }

    out
}

/// Public wrapper used by the UI tree: returns the attr lines as a `Vec<String>`.
/// Each line is already sensitive/unknown-masked and truncated; limit to
/// [`MAX_ATTRS_PER_RESOURCE`] in the caller.
pub fn digest_attrs_public(actions: &[String], change: &crate::plan::Change) -> Vec<String> {
    attr_lines(actions, change)
}

/// Digest the change body into a compact `(attr: old -> new, ...)` fragment
/// for a single resource.
fn digest_attrs(actions: &[String], change: &crate::plan::Change) -> String {
    let parts = attr_lines(actions, change);
    if parts.is_empty() {
        String::new()
    } else {
        format!("({})", parts.join(", "))
    }
}

/// Shared attribute-line construction used by both the LLM digest and the UI tree.
fn attr_lines(actions: &[String], change: &crate::plan::Change) -> Vec<String> {
    let action = match Action::from_actions(actions) {
        Ok(a) => a,
        Err(_) => return vec!["(unknown action)".into()],
    };

    let before = change.before.as_ref().and_then(Value::as_object);
    let after = change.after.as_ref().and_then(Value::as_object);
    let sensitive_after = sensitive_key_set(change.after_sensitive.as_ref());
    let sensitive_before = sensitive_key_set(change.before_sensitive.as_ref());
    let unknown_after = unknown_key_set(change.after_unknown.as_ref());

    let mut parts: Vec<String> = Vec::new();

    match action {
        Action::Create => {
            if let Some(a) = after {
                for (k, v) in a.iter().take(MAX_ATTRS_PER_RESOURCE) {
                    parts.push(format!(
                        "{k}={}",
                        masked_value(k, v, &sensitive_after, &unknown_after)
                    ));
                }
            }
        }
        Action::Delete => {
            if let Some(b) = before {
                for (k, v) in b.iter().take(MAX_ATTRS_PER_RESOURCE) {
                    parts.push(format!(
                        "{k}={}",
                        masked_value(k, v, &sensitive_before, &Default::default())
                    ));
                }
            }
        }
        Action::Update | Action::Replace => {
            let (b, a) = (before.unwrap_or(&EMPTY_MAP), after.unwrap_or(&EMPTY_MAP));
            let mut diffs: Vec<(String, String)> = Vec::new();
            for (k, av) in a {
                let bv = b.get(k).unwrap_or(&Value::Null);
                if bv != av {
                    let masked_before = masked_value(k, bv, &sensitive_before, &Default::default());
                    let masked_after = masked_value(k, av, &sensitive_after, &unknown_after);
                    diffs.push((k.clone(), format!("{masked_before} -> {masked_after}")));
                }
            }
            for (k, v) in b {
                if !a.contains_key(k) {
                    let masked_before = masked_value(k, v, &sensitive_before, &Default::default());
                    diffs.push((k.clone(), format!("{masked_before} -> (removed)")));
                }
            }
            for (k, rendered) in diffs.into_iter().take(MAX_ATTRS_PER_RESOURCE) {
                parts.push(format!("{k}: {rendered}"));
            }
        }
        Action::Read | Action::NoOp => {}
    }

    parts
}

use std::sync::LazyLock;
static EMPTY_MAP: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);

/// Collect attribute names flagged `true` in a sensitive-values object.
fn sensitive_key_set(v: Option<&Value>) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    match v {
        Some(Value::Object(obj)) => {
            for (k, val) in obj {
                if matches!(val, Value::Bool(true)) || matches!(val, Value::Object(_)) {
                    out.insert(k.clone());
                }
            }
        }
        Some(Value::Bool(true)) => {
            out.insert("*".into());
        }
        _ => {}
    }
    out
}

/// Collect attribute names flagged unknown in `after_unknown`.
fn unknown_key_set(v: Option<&Value>) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    if let Some(Value::Object(obj)) = v {
        for (k, val) in obj {
            if matches!(val, Value::Bool(true)) {
                out.insert(k.clone());
            }
        }
    }
    out
}

fn masked_value(
    key: &str,
    v: &Value,
    sensitive: &std::collections::HashSet<String>,
    unknown: &std::collections::HashSet<String>,
) -> String {
    if sensitive.contains(key) || sensitive.contains("*") {
        return SENSITIVE_PLACEHOLDER.into();
    }
    if unknown.contains(key) {
        return "(known after apply)".into();
    }
    render_value(v)
}

/// Render a JSON value as a compact string, truncating long strings.
fn render_value(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => {
            if s.len() > 40 {
                format!("\"{}...\"", &s[..37])
            } else {
                format!("\"{s}\"")
            }
        }
        Value::Array(a) => format!("[{} items]", a.len()),
        Value::Object(o) => format!("{{{} keys}}", o.len()),
    }
}

/// Rough token estimate: ~4 chars/token (OpenAI BPE average for English/code).
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Digest, falling back to group-counts-only if the full version exceeds the token budget.
pub fn digest_with_budget(plan: &Plan, groups: &[Group]) -> String {
    let full = digest_plan(plan, groups);
    if estimate_tokens(&full) <= MAX_DIGEST_TOKENS {
        full
    } else {
        group_counts_only(plan, groups)
    }
}

fn group_counts_only(plan: &Plan, groups: &[Group]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Plan with {} resource change(s). Group counts only (full digest exceeded budget).\n\n",
        plan.total_changes()
    ));
    for g in groups {
        out.push_str(&format!(
            "{} {} ({})\n",
            g.action.glyph(),
            g.resource_type,
            g.count()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::groups::group_plan;

    fn mk_plan(json: &str) -> Plan {
        Plan::parse(json).unwrap()
    }

    #[test]
    fn digest_includes_resource_lines() {
        let p = mk_plan(
            r#"{
            "format_version":"1.2",
            "resource_changes":[
                {"address":"aws_dynamodb_table.users","type":"aws_dynamodb_table",
                 "change":{"actions":["create"],"after":{"billing_mode":"PAY_PER_REQUEST","hash_key":"id"}}}
            ]
        }"#,
        );
        let g = group_plan(&p, false);
        let d = digest_plan(&p, &g);
        assert!(d.contains("+ aws_dynamodb_table.users"));
        assert!(d.contains("billing_mode"));
        assert!(d.contains("PAY_PER_REQUEST"));
    }

    #[test]
    fn sensitive_values_stripped() {
        let p = mk_plan(
            r#"{
            "format_version":"1.2",
            "resource_changes":[
                {"address":"aws_db_instance.x","type":"aws_db_instance",
                 "change":{
                    "actions":["create"],
                    "after":{"password":"hunter2","name":"prod"},
                    "after_sensitive":{"password":true}
                 }}
            ]
        }"#,
        );
        let g = group_plan(&p, false);
        let d = digest_plan(&p, &g);
        assert!(d.contains("<sensitive>"));
        assert!(!d.contains("hunter2"));
        assert!(d.contains("\"prod\""));
    }

    #[test]
    fn update_shows_only_diffed_attrs() {
        let p = mk_plan(
            r#"{
            "format_version":"1.2",
            "resource_changes":[
                {"address":"aws_security_group.web","type":"aws_security_group",
                 "change":{"actions":["update"],
                    "before":{"name":"web","description":"old"},
                    "after":{"name":"web","description":"new"}
                 }}
            ]
        }"#,
        );
        let g = group_plan(&p, false);
        let d = digest_plan(&p, &g);
        assert!(d.contains("description"));
        assert!(d.contains("\"old\" -> \"new\""));
        assert!(!d.contains("name:"));
    }

    #[test]
    fn replace_action_shows_diffs() {
        let p = mk_plan(
            r#"{
            "format_version":"1.2",
            "resource_changes":[
                {"address":"aws_instance.web","type":"aws_instance",
                 "change":{"actions":["delete","create"],
                    "before":{"ami":"ami-old"},
                    "after":{"ami":"ami-new"}
                 }}
            ]
        }"#,
        );
        let g = group_plan(&p, false);
        let d = digest_plan(&p, &g);
        assert!(d.contains("-/+ aws_instance.web"));
        assert!(d.contains("ami"));
    }

    #[test]
    fn empty_plan_digest() {
        let p = mk_plan(r#"{"format_version":"1.2","resource_changes":[]}"#);
        let g = group_plan(&p, false);
        let d = digest_plan(&p, &g);
        assert!(d.contains("0 to add"));
        assert!(d.contains("0 to destroy"));
    }

    #[test]
    fn token_budget_fallback() {
        // Build a plan with > MAX_DIGEST_TOKENS worth of text, but < FULL_DIGEST_LIMIT resources.
        let mut changes = String::new();
        for i in 0..100 {
            if i > 0 {
                changes.push(',');
            }
            let long_attr = "x".repeat(200);
            changes.push_str(&format!(
                r#"{{"address":"t.r{i}","type":"t","change":{{"actions":["create"],"after":{{"k":"{long_attr}"}}}}}}"#
            ));
        }
        let raw = format!(r#"{{"format_version":"1.2","resource_changes":[{changes}]}}"#);
        let p = Plan::parse(&raw).unwrap();
        let g = group_plan(&p, false);
        let full = digest_plan(&p, &g);
        let budgeted = digest_with_budget(&p, &g);
        if estimate_tokens(&full) > MAX_DIGEST_TOKENS {
            assert!(budgeted.contains("Group counts only"));
            assert!(estimate_tokens(&budgeted) < estimate_tokens(&full));
        } else {
            assert_eq!(budgeted, full);
        }
    }

    #[test]
    fn group_counts_fallback_for_mega_plan() {
        // Build a plan with >= FULL_DIGEST_LIMIT resources.
        let mut changes = String::new();
        for i in 0..FULL_DIGEST_LIMIT + 10 {
            if i > 0 {
                changes.push(',');
            }
            changes.push_str(&format!(
                r#"{{"address":"t.r{i}","type":"t","change":{{"actions":["create"]}}}}"#
            ));
        }
        let raw = format!(r#"{{"format_version":"1.2","resource_changes":[{changes}]}}"#);
        let p = Plan::parse(&raw).unwrap();
        let g = group_plan(&p, false);
        let d = digest_plan(&p, &g);
        assert!(d.contains("detailed per-resource digest omitted"));
        assert!(!d.contains("t.r1 ("));
    }
}
