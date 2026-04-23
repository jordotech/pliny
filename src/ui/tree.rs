//! Flat row model for the tree list.
//!
//! Rows are rebuilt from [`crate::groups::Group`] + expansion state every frame.
//! Cheap because group counts are small; no need for a persistent tree structure.

use crate::digest::{MAX_ATTRS_PER_RESOURCE, digest_attrs_public};
use crate::groups::Group;
use crate::plan::Action;
use ratatui::style::{Color, Modifier, Style};
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct Row {
    pub depth: usize,
    pub label: String,
    pub kind: RowKind,
    pub action: Action,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // attr_idx reserved for future per-attr expand/copy operations.
pub enum RowKind {
    Group { group_idx: usize },
    Resource { group_idx: usize, resource_idx: usize },
    Attr { group_idx: usize, resource_idx: usize, attr_idx: usize },
}

impl Row {
    pub fn style(&self) -> Style {
        let fg = match self.action {
            Action::Delete => Color::Red,
            Action::Replace => Color::Magenta,
            Action::Update => Color::Yellow,
            Action::Create => Color::Green,
            Action::Read => Color::Cyan,
            Action::NoOp => Color::Gray,
        };
        let mut s = Style::default().fg(fg);
        if matches!(self.kind, RowKind::Group { .. }) {
            s = s.add_modifier(Modifier::BOLD);
        }
        s
    }
}

/// Build the flat row list. Groups always shown; resources shown if group
/// expanded; attrs shown if resource expanded.
pub fn build_rows(
    groups: &[Group],
    expanded_groups: &HashSet<usize>,
    expanded_resources: &HashSet<(usize, usize)>,
) -> Vec<Row> {
    let mut rows = Vec::new();
    for (gi, group) in groups.iter().enumerate() {
        rows.push(Row {
            depth: 0,
            label: group.label(),
            kind: RowKind::Group { group_idx: gi },
            action: group.action,
        });
        if !expanded_groups.contains(&gi) {
            continue;
        }
        for (ri, rc) in group.changes.iter().enumerate() {
            rows.push(Row {
                depth: 1,
                label: format!("{} {}", group.action.glyph(), rc.address),
                kind: RowKind::Resource {
                    group_idx: gi,
                    resource_idx: ri,
                },
                action: group.action,
            });
            if !expanded_resources.contains(&(gi, ri)) {
                continue;
            }
            let attr_lines = digest_attrs_public(&rc.change.actions, &rc.change);
            for (ai, line) in attr_lines.iter().take(MAX_ATTRS_PER_RESOURCE).enumerate() {
                rows.push(Row {
                    depth: 2,
                    label: line.clone(),
                    kind: RowKind::Attr {
                        group_idx: gi,
                        resource_idx: ri,
                        attr_idx: ai,
                    },
                    action: group.action,
                });
            }
            if attr_lines.is_empty() {
                rows.push(Row {
                    depth: 2,
                    label: "(no diffed attributes)".into(),
                    kind: RowKind::Attr {
                        group_idx: gi,
                        resource_idx: ri,
                        attr_idx: 0,
                    },
                    action: group.action,
                });
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::groups::group_plan;
    use crate::plan::Plan;

    fn mk_plan(json: &str) -> Plan {
        Plan::parse(json).unwrap()
    }

    #[test]
    fn collapsed_shows_only_groups() {
        let p = mk_plan(
            r#"{
            "format_version":"1.2",
            "resource_changes":[
                {"address":"aws_dynamodb_table.users","type":"aws_dynamodb_table","change":{"actions":["create"]}}
            ]
        }"#,
        );
        let g = group_plan(&p, false);
        let rows = build_rows(&g, &HashSet::new(), &HashSet::new());
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0].kind, RowKind::Group { .. }));
    }

    #[test]
    fn expanded_group_shows_resources() {
        let p = mk_plan(
            r#"{
            "format_version":"1.2",
            "resource_changes":[
                {"address":"aws_dynamodb_table.users","type":"aws_dynamodb_table","change":{"actions":["create"]}},
                {"address":"aws_dynamodb_table.orders","type":"aws_dynamodb_table","change":{"actions":["create"]}}
            ]
        }"#,
        );
        let g = group_plan(&p, false);
        let mut expanded = HashSet::new();
        expanded.insert(0);
        let rows = build_rows(&g, &expanded, &HashSet::new());
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0].kind, RowKind::Group { .. }));
        assert!(matches!(rows[1].kind, RowKind::Resource { .. }));
        assert!(matches!(rows[2].kind, RowKind::Resource { .. }));
    }

    #[test]
    fn expanded_resource_shows_attrs() {
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
        let mut eg = HashSet::new();
        eg.insert(0);
        let mut er = HashSet::new();
        er.insert((0, 0));
        let rows = build_rows(&g, &eg, &er);
        // Group + resource + 2 attrs = 4
        assert_eq!(rows.len(), 4);
        assert!(matches!(rows[0].kind, RowKind::Group { .. }));
        assert!(matches!(rows[1].kind, RowKind::Resource { .. }));
        assert!(matches!(rows[2].kind, RowKind::Attr { .. }));
        assert!(matches!(rows[3].kind, RowKind::Attr { .. }));
    }
}
