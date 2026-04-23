//! Group resource changes by (resource_type, action) for tree rendering.
//!
//! Ordering: actions sorted by destructive-potential priority (Delete first,
//! then Replace, Update, Create, Read, NoOp). Within an action bucket,
//! resource types sorted alphabetically for stable rendering.

use crate::plan::{Action, Plan, ResourceChange};

#[derive(Debug, Clone)]
pub struct Group {
    pub resource_type: String,
    pub action: Action,
    pub changes: Vec<ResourceChange>,
}

impl Group {
    pub fn count(&self) -> usize {
        self.changes.len()
    }

    pub fn label(&self) -> String {
        format!(
            "{} {} ({})",
            self.action.glyph(),
            self.resource_type,
            self.count()
        )
    }
}

/// Group a plan's changes. If `show_noops` is false, no-op entries are excluded.
pub fn group_plan(plan: &Plan, show_noops: bool) -> Vec<Group> {
    let mut all: Vec<&ResourceChange> = plan.resource_changes.iter().collect();
    all.extend(plan.resource_drift.iter());

    let mut buckets: std::collections::BTreeMap<(Action, String), Vec<ResourceChange>> =
        std::collections::BTreeMap::new();

    for rc in all {
        let Ok(action) = Action::from_actions(&rc.change.actions) else {
            continue;
        };
        if action == Action::NoOp && !show_noops {
            continue;
        }
        buckets
            .entry((action, rc.resource_type.clone()))
            .or_default()
            .push(rc.clone());
    }

    let mut groups: Vec<Group> = buckets
        .into_iter()
        .map(|((action, resource_type), changes)| Group {
            resource_type,
            action,
            changes,
        })
        .collect();

    // Primary sort: action priority. Secondary: resource_type alphabetical.
    groups.sort_by(|a, b| {
        action_priority(a.action)
            .cmp(&action_priority(b.action))
            .then_with(|| a.resource_type.cmp(&b.resource_type))
    });

    // Within each group, sort changes by address for stable display.
    for g in &mut groups {
        g.changes.sort_by(|a, b| a.address.cmp(&b.address));
    }

    groups
}

/// Lower number = higher priority = rendered first.
/// Destructive actions surface at the top so the user sees risk immediately.
fn action_priority(a: Action) -> u8 {
    match a {
        Action::Delete => 0,
        Action::Replace => 1,
        Action::Update => 2,
        Action::Create => 3,
        Action::Read => 4,
        Action::NoOp => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_plan(json: &str) -> Plan {
        Plan::parse(json).unwrap()
    }

    #[test]
    fn groups_by_type_and_action() {
        let p = mk_plan(
            r#"{
            "format_version":"1.2",
            "resource_changes":[
                {"address":"aws_dynamodb_table.users","type":"aws_dynamodb_table","change":{"actions":["create"]}},
                {"address":"aws_dynamodb_table.orders","type":"aws_dynamodb_table","change":{"actions":["create"]}},
                {"address":"aws_security_group.web","type":"aws_security_group","change":{"actions":["update"]}}
            ]
        }"#,
        );
        let groups = group_plan(&p, false);
        assert_eq!(groups.len(), 2);
        // Update comes before Create in priority
        assert_eq!(groups[0].action, Action::Update);
        assert_eq!(groups[0].resource_type, "aws_security_group");
        assert_eq!(groups[0].count(), 1);
        assert_eq!(groups[1].action, Action::Create);
        assert_eq!(groups[1].resource_type, "aws_dynamodb_table");
        assert_eq!(groups[1].count(), 2);
    }

    #[test]
    fn hides_noops_by_default() {
        let p = mk_plan(
            r#"{
            "format_version":"1.2",
            "resource_changes":[
                {"address":"a.b","type":"t","change":{"actions":["no-op"]}},
                {"address":"a.c","type":"t","change":{"actions":["create"]}}
            ]
        }"#,
        );
        let hidden = group_plan(&p, false);
        assert_eq!(hidden.len(), 1);
        assert_eq!(hidden[0].action, Action::Create);

        let shown = group_plan(&p, true);
        assert_eq!(shown.len(), 2);
    }

    #[test]
    fn destroys_surface_first() {
        let p = mk_plan(
            r#"{
            "format_version":"1.2",
            "resource_changes":[
                {"address":"a.created","type":"t","change":{"actions":["create"]}},
                {"address":"a.deleted","type":"t","change":{"actions":["delete"]}},
                {"address":"a.replaced","type":"t","change":{"actions":["delete","create"]}}
            ]
        }"#,
        );
        let groups = group_plan(&p, false);
        let actions: Vec<Action> = groups.iter().map(|g| g.action).collect();
        assert_eq!(actions, vec![Action::Delete, Action::Replace, Action::Create]);
    }

    #[test]
    fn label_format() {
        let p = mk_plan(
            r#"{
            "format_version":"1.2",
            "resource_changes":[
                {"address":"a.x","type":"aws_dynamodb_table","change":{"actions":["create"]}},
                {"address":"a.y","type":"aws_dynamodb_table","change":{"actions":["create"]}},
                {"address":"a.z","type":"aws_dynamodb_table","change":{"actions":["create"]}}
            ]
        }"#,
        );
        let groups = group_plan(&p, false);
        assert_eq!(groups[0].label(), "+ aws_dynamodb_table (3)");
    }

    #[test]
    fn includes_resource_drift_in_groups() {
        let p = mk_plan(
            r#"{
            "format_version":"1.2",
            "resource_changes":[],
            "resource_drift":[
                {"address":"aws_instance.web","type":"aws_instance","change":{"actions":["update"]}}
            ]
        }"#,
        );
        let groups = group_plan(&p, false);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].resource_type, "aws_instance");
    }
}
