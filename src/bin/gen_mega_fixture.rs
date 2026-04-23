//! Generate a synthetic terraform plan JSON for load testing.
//!
//! Writes to stdout. Usage:
//!   cargo run --bin gen-mega-fixture -- [resource_count] > fixtures/mega.json
//!
//! Defaults: 800 resources across 10 resource types, ~20% flagged sensitive,
//! ~10% replace actions. Good enough to exercise the group-counts fallback
//! and mask paths without hitting a real cloud.

use serde_json::{Value, json};
use std::io::Write;

const DEFAULT_COUNT: usize = 800;
const RESOURCE_TYPES: &[&str] = &[
    "aws_dynamodb_table",
    "aws_s3_bucket",
    "aws_iam_role",
    "aws_lambda_function",
    "aws_security_group",
    "aws_rds_cluster",
    "aws_instance",
    "aws_cloudwatch_log_group",
    "aws_sqs_queue",
    "aws_kms_key",
];

fn main() {
    let count: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_COUNT);

    let mut changes: Vec<Value> = Vec::with_capacity(count);
    for i in 0..count {
        let rt = RESOURCE_TYPES[i % RESOURCE_TYPES.len()];
        let sensitive = i % 5 == 0;
        let replace = i % 10 == 0;

        let (actions, before, after, before_sensitive, after_sensitive) = if replace {
            (
                vec!["delete", "create"],
                json!({"id": format!("old-{i}"), "name": format!("res-{i}")}),
                json!({"id": format!("new-{i}"), "name": format!("res-{i}")}),
                json!({}),
                json!({}),
            )
        } else if sensitive {
            (
                vec!["create"],
                Value::Null,
                json!({"name": format!("res-{i}"), "password": "redacted"}),
                json!({}),
                json!({"password": true}),
            )
        } else {
            (
                vec!["create"],
                Value::Null,
                json!({"name": format!("res-{i}"), "tier": "standard"}),
                json!({}),
                json!({}),
            )
        };

        changes.push(json!({
            "address": format!("{rt}.r{i}"),
            "type": rt,
            "change": {
                "actions": actions,
                "before": before,
                "after": after,
                "before_sensitive": before_sensitive,
                "after_sensitive": after_sensitive,
            }
        }));
    }

    let plan = json!({
        "format_version": "1.2",
        "resource_changes": changes,
    });

    let out = std::io::stdout();
    let mut w = out.lock();
    serde_json::to_writer_pretty(&mut w, &plan).expect("serialize");
    let _ = writeln!(w);
}
