//! Benchmark round 3 harness ("Q-chain question"): given a statement id,
//! ONE invocation reports everything an agent needs to answer "what does
//! this statement do and who calls it" -- matching statement(s) with
//! defining file, dialect-variant presence/absence (explicitly, not by
//! omission), flattened SQL (so table names are visible in the text), and
//! Java caller occurrences (file:line, comment lines excluded -- same
//! string-scan rule as the rg baseline: an occurrence counts only if the
//! matched line's trimmed content doesn't start with a comment marker).
//!
//! Scratch tooling, not part of the published library or its quality
//! gates. Corpus paths are private -- supplied via env vars, never
//! hardcoded or committed. Same convention as examples/k1_harness.rs.
//!
//! Run with:
//!   IBATIS_BATCH_DIR=<path to the private batch repo root> \
//!   cargo run --release --example q_chain -- <statement.id>
//!
//! Assumes the corpus layout of that private repo (its paths supplied via
//! env, never committed):
//! - sqlMaps: $IBATIS_BATCH_DIR/$IBATIS_SQLMAP_SUBPATH/<dialect>/**/*.xml,
//!   where IBATIS_SQLMAP_SUBPATH is the project-specific path (under the repo
//!   root) to the directory holding the per-dialect sqlMap subdirs. Dialect
//!   subdirectory names, e.g. `mysql`/`oracle`, are discovered from the
//!   filesystem -- never hardcoded, so an id absent from a dialect dir is
//!   reported as ABSENT rather than silently omitted.
//! - Java sources: $IBATIS_BATCH_DIR/src/main/java/**/*.java
//!
//! Output is designed for the byte budget an agent would actually read:
//! terse text, no decorative headers, and flattened SQL variants beyond
//! the first few are collapsed into an explicit "(+n more variants)"
//! marker rather than printed in full.

use batis_xml::SqlText;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_VARIANTS: usize = 3;

fn walk_files(root: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_files(&path, ext, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(path);
        }
    }
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn rel<'a>(path: &'a Path, root: &Path) -> &'a Path {
    path.strip_prefix(root).unwrap_or(path)
}

fn main() {
    let id = match env::args().nth(1) {
        Some(id) => id,
        None => {
            eprintln!(
                "usage: IBATIS_BATCH_DIR=<path> IBATIS_SQLMAP_SUBPATH=<subpath> cargo run --release --example q_chain -- <statement.id>"
            );
            std::process::exit(1);
        }
    };

    let batch_dir = match env::var("IBATIS_BATCH_DIR") {
        Ok(v) => PathBuf::from(v),
        Err(_) => {
            eprintln!("IBATIS_BATCH_DIR is not set");
            std::process::exit(1);
        }
    };

    // The path under the repo root to the per-dialect sqlMap directory is
    // project-specific; supply it via env so no private layout is committed.
    let sqlmap_subpath = match env::var("IBATIS_SQLMAP_SUBPATH") {
        Ok(v) => v,
        Err(_) => {
            eprintln!(
                "IBATIS_SQLMAP_SUBPATH is not set (path under the repo root to the per-dialect sqlMap directory)"
            );
            std::process::exit(1);
        }
    };
    let sqlmap_root = batch_dir.join(&sqlmap_subpath);
    let java_root = batch_dir.join("src/main/java");

    // Discover dialect dirs from the filesystem -- never hardcode names, so
    // absence from a dialect is a real finding, not an assumption.
    let mut dialect_dirs: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(&sqlmap_root) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    dialect_dirs.push(name.to_string());
                }
            }
        }
    }
    dialect_dirs.sort();

    struct Found {
        dialect: String,
        path: PathBuf,
        kind: batis_xml::StatementKind,
        database_id: Option<String>,
        sql: SqlText,
    }

    let mut found: Vec<Found> = Vec::new();
    let mut present: BTreeSet<String> = BTreeSet::new();

    for dialect in &dialect_dirs {
        let mut xml_files = Vec::new();
        walk_files(&sqlmap_root.join(dialect), "xml", &mut xml_files);
        for path in xml_files {
            let Ok(bytes) = fs::read(&path) else { continue };
            let result = batis_xml::parse_bytes(&bytes);
            let Some(mapper) = result.mapper else {
                continue;
            };
            for stmt in mapper.statements {
                if stmt.id.as_ref().map(|s| s.value.as_str()) == Some(id.as_str()) {
                    present.insert(dialect.clone());
                    found.push(Found {
                        dialect: dialect.clone(),
                        path: path.clone(),
                        kind: stmt.kind,
                        database_id: stmt.database_id.map(|d| d.value),
                        sql: stmt.sql,
                    });
                }
            }
        }
    }

    println!("id: {id}");
    println!("matches: {}", found.len());
    for dialect in &dialect_dirs {
        if present.contains(dialect) {
            println!("dialect {dialect}: present");
        } else {
            println!("dialect {dialect}: ABSENT");
        }
    }

    for f in &found {
        println!(
            "-- [{}] {} (kind={:?} databaseId={:?}) --",
            f.dialect,
            rel(&f.path, &batch_dir).display(),
            f.kind,
            f.database_id
        );
        match &f.sql {
            SqlText::Variants(variants) => {
                let total = variants.len();
                for (i, v) in variants.iter().take(MAX_VARIANTS).enumerate() {
                    let cond = if v.conditions.is_empty() {
                        "always".to_string()
                    } else {
                        v.conditions.join(" && ")
                    };
                    println!("  v{} [{cond}]: {}", i + 1, collapse_ws(&v.text.text));
                }
                if total > MAX_VARIANTS {
                    println!("  (+{} more variants)", total - MAX_VARIANTS);
                }
            }
            SqlText::Union { text, branch_count } => {
                println!(
                    "  union[{branch_count} branches over cap]: {}",
                    collapse_ws(&text.text)
                );
            }
            // SqlText is #[non_exhaustive]: a future representation must
            // not break existing consumers at compile time.
            _ => println!("  [unrecognized SqlText representation]"),
        }
    }

    // Java callers: same string-scan rule as the rg baseline -- a line
    // matches if it contains the id verbatim and isn't a comment line.
    let mut java_files = Vec::new();
    walk_files(&java_root, "java", &mut java_files);
    let mut callers: Vec<(PathBuf, usize)> = Vec::new();
    for path in &java_files {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        for (lineno, line) in content.lines().enumerate() {
            if !line.contains(id.as_str()) {
                continue;
            }
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
                continue;
            }
            callers.push((path.clone(), lineno + 1));
        }
    }
    let file_count = callers
        .iter()
        .map(|(p, _)| p)
        .collect::<BTreeSet<_>>()
        .len();
    println!("java callers: {file_count} files / {} lines", callers.len());
    for (path, lineno) in &callers {
        println!("  {}:{lineno}", rel(path, &batch_dir).display());
    }
}
