//! Prints, for one or more mapper XML files, each statement's
//! kind/id/databaseId plus its flattened SQL variants with their
//! activating conditions -- the "what actually runs" view the crate
//! exists to produce. Doubles as the base for the benchmark round-3
//! harness (answer a statement/binding question from this output alone,
//! not by re-reading the raw XML).
//!
//! Run with: cargo run --example dump_statements -- path/to/Mapper.xml [more paths...]

use batis_xml::{SqlText, StatementKind};
use std::env;
use std::fs;

fn kind_label(kind: StatementKind) -> &'static str {
    match kind {
        StatementKind::Select => "select",
        StatementKind::Insert => "insert",
        StatementKind::Update => "update",
        StatementKind::Delete => "delete",
        StatementKind::Procedure => "procedure",
        StatementKind::Generic => "generic",
    }
}

fn main() {
    let paths: Vec<String> = env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: dump_statements <mapper.xml> [more paths...]");
        std::process::exit(1);
    }

    for path in &paths {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(err) => {
                eprintln!("{path}: read error: {err}");
                continue;
            }
        };
        let result = batis_xml::parse_bytes(&bytes);
        println!("== {path} ({:?}) ==", result.dialect);

        let Some(mapper) = &result.mapper else {
            println!("  (no mapper root -- see diagnostics)");
            continue;
        };

        for stmt in &mapper.statements {
            let id = stmt
                .id
                .as_ref()
                .map(|s| s.value.as_str())
                .unwrap_or("<missing id>");
            let db = stmt
                .database_id
                .as_ref()
                .map(|s| format!(" databaseId={}", s.value))
                .unwrap_or_default();
            println!("- {} {}{}", kind_label(stmt.kind), id, db);
            match &stmt.sql {
                SqlText::Variants(variants) => {
                    for v in variants {
                        let cond = if v.conditions.is_empty() {
                            "always".to_string()
                        } else {
                            v.conditions.join(" && ")
                        };
                        println!("    [{cond}] {}", v.text.text.trim());
                    }
                }
                SqlText::Union { text, branch_count } => {
                    println!(
                        "    [union, {branch_count} branches over cap] {}",
                        text.text.trim()
                    );
                }
            }
        }

        if !result.diagnostics.is_empty() {
            println!("  diagnostics:");
            for d in &result.diagnostics {
                println!("    {:?}: {}", d.code, d.message);
            }
        }
    }
}
