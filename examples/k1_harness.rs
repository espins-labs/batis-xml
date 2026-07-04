//! K-1 corpus measurement harness — scratch tooling, not part of the
//! published library or its quality gates.
//!
//! Measures batis-xml against a private real-world corpus (paths supplied
//! via env vars, never hardcoded or committed):
//! - `MYBATIS_ADMIN_DIR`, `MYBATIS_API_DIR`: MyBatis mapper corpora
//!   (each expected to contain `src/main/resources/**/*.xml` mappers and
//!   a mirroring `src/main/java` interface tree).
//! - `IBATIS_BATCH_DIR`: iBatis sqlMap corpus (`src/main/resources/**/*.xml`
//!   + `src/main/java`).
//! - `K1_OUTPUT_DIR` (optional): where the per-file failure-bucket report
//!   is written. Defaults to a temp dir — never inside this repo, since
//!   the corpus paths and raw per-file results are private.
//!
//! Run with: `MYBATIS_ADMIN_DIR=... MYBATIS_API_DIR=... IBATIS_BATCH_DIR=...
//! cargo run --example k1_harness`
//!
//! Only aggregate numbers and bucket *categories* belong in a status
//! report — never file paths or corpus content.

use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

struct CorpusResult {
    files_seen: usize,
    parsed_ok: usize,
    ground_truth_statements: usize,
    matched_statements: usize,
    bound_statements: usize,
    total_statements: usize,
    diagnostic_counts: HashMap<String, usize>,
    buckets: Vec<(String, String)>, // (file, reason)
}

impl CorpusResult {
    fn new() -> Self {
        CorpusResult {
            files_seen: 0,
            parsed_ok: 0,
            ground_truth_statements: 0,
            matched_statements: 0,
            bound_statements: 0,
            total_statements: 0,
            diagnostic_counts: HashMap::new(),
            buckets: Vec::new(),
        }
    }
}

fn walk_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("xml") {
            out.push(path);
        }
    }
}

fn load_java_sources(java_root: &Path) -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    walk_java(java_root, &mut files);
    files
        .into_iter()
        .filter_map(|p| fs::read_to_string(&p).ok().map(|c| (p, c)))
        .collect()
}

fn walk_java(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_java(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("java") {
            out.push(path);
        }
    }
}

fn measure_mybatis(dirs: &[(&str, PathBuf)]) -> CorpusResult {
    let stmt_ground_truth =
        Regex::new(r"(?i)<(select|insert|update|delete)\b[^>]*\bid\s*=").unwrap();

    let mut result = CorpusResult::new();

    for (label, dir) in dirs {
        let resource_root = dir.join("src/main/resources");
        let java_root = dir.join("src/main/java");
        let java_sources = load_java_sources(&java_root);
        // Index Java sources by "package/Class" relative path (without
        // .java) for O(1) namespace -> file lookup.
        let by_relpath: HashMap<String, &str> = java_sources
            .iter()
            .filter_map(|(p, content)| {
                let rel = p.strip_prefix(&java_root).ok()?;
                let rel = rel.to_str()?.trim_end_matches(".java").replace('\\', "/");
                Some((rel, content.as_str()))
            })
            .collect();

        let mut xml_files = Vec::new();
        walk_files(&resource_root, &mut xml_files);

        for path in xml_files {
            let Ok(raw) = fs::read(&path) else { continue };
            let Ok(raw_text) = String::from_utf8(raw.clone()) else {
                continue;
            };
            if !raw_text.contains("<mapper ") {
                continue; // not a MyBatis mapper file (config, spring xml, ...)
            }
            result.files_seen += 1;

            let ground_truth = stmt_ground_truth.find_iter(&raw_text).count();
            result.ground_truth_statements += ground_truth;

            let parsed = batis_xml::parse_bytes(&raw);
            for d in &parsed.diagnostics {
                *result
                    .diagnostic_counts
                    .entry(format!("{:?}", d.code))
                    .or_insert(0) += 1;
            }

            let Some(mapper) = parsed.mapper else {
                result.buckets.push((
                    format!("{label}:{}", path.display()),
                    "no_mapper_parsed".to_string(),
                ));
                continue;
            };
            result.parsed_ok += 1;

            let matched = mapper.statements.iter().filter(|s| s.id.is_some()).count();
            result.matched_statements += matched;

            let Some(namespace) = &mapper.namespace else {
                result.buckets.push((
                    format!("{label}:{}", path.display()),
                    "missing_namespace".to_string(),
                ));
                continue;
            };
            let relpath = namespace.value.replace('.', "/");
            let Some(java_content) = by_relpath.get(relpath.as_str()) else {
                result.buckets.push((
                    format!("{label}:{}", path.display()),
                    "no_matching_interface_file".to_string(),
                ));
                continue;
            };

            for stmt in &mapper.statements {
                let Some(id) = &stmt.id else { continue };
                result.total_statements += 1;
                // A plain substring check for "<id>(" is good enough for
                // this scratch measurement (no need to compile a regex per
                // statement); a false positive from e.g. a longer method
                // name sharing this suffix is well within noise margin.
                let bound = java_content.contains(&format!("{}(", id.value));
                if bound {
                    result.bound_statements += 1;
                } else {
                    result.buckets.push((
                        format!("{label}:{}", path.display()),
                        format!("unbound_statement:{}", id.value),
                    ));
                }
            }
        }
    }

    result
}

fn measure_ibatis(dir: &Path) -> CorpusResult {
    let stmt_ground_truth =
        Regex::new(r"(?i)<(select|insert|update|delete|procedure|statement)\b[^>]*\bid\s*=")
            .unwrap();
    let sqlmap_root = Regex::new(r"<sqlMap(\s|>)").unwrap();

    let mut result = CorpusResult::new();
    let resource_root = dir.join("src/main/resources");
    let java_root = dir.join("src/main/java");
    let java_sources = load_java_sources(&java_root);

    let mut xml_files = Vec::new();
    walk_files(&resource_root, &mut xml_files);

    for path in xml_files {
        let Ok(raw) = fs::read(&path) else { continue };
        let Ok(raw_text) = String::from_utf8(raw.clone()) else {
            continue;
        };
        if !sqlmap_root.is_match(&raw_text) || raw_text.contains("sqlMapConfig") {
            continue; // config file, not a per-mapper sqlMap
        }
        result.files_seen += 1;

        let ground_truth = stmt_ground_truth.find_iter(&raw_text).count();
        result.ground_truth_statements += ground_truth;

        let parsed = batis_xml::parse_bytes(&raw);
        for d in &parsed.diagnostics {
            *result
                .diagnostic_counts
                .entry(format!("{:?}", d.code))
                .or_insert(0) += 1;
        }

        let Some(mapper) = parsed.mapper else {
            result
                .buckets
                .push((path.display().to_string(), "no_mapper_parsed".to_string()));
            continue;
        };
        result.parsed_ok += 1;

        let matched = mapper.statements.iter().filter(|s| s.id.is_some()).count();
        result.matched_statements += matched;

        for stmt in &mapper.statements {
            let Some(id) = &stmt.id else { continue };
            result.total_statements += 1;
            let literal = format!("\"{}\"", id.value);
            let bound = java_sources
                .iter()
                .any(|(_, content)| content.contains(&literal));
            if bound {
                result.bound_statements += 1;
            } else {
                result.buckets.push((
                    path.display().to_string(),
                    format!("unbound_statement:{}", id.value),
                ));
            }
        }
    }

    result
}

fn print_summary(name: &str, r: &CorpusResult) {
    println!("=== {name} ===");
    println!("files_seen: {}", r.files_seen);
    println!(
        "parse_success_rate: {}/{} ({:.1}%)",
        r.parsed_ok,
        r.files_seen,
        pct(r.parsed_ok, r.files_seen)
    );
    println!(
        "statement_recall: {}/{} ({:.1}%)",
        r.matched_statements,
        r.ground_truth_statements,
        pct(r.matched_statements, r.ground_truth_statements)
    );
    println!(
        "binding_rate: {}/{} ({:.1}%)",
        r.bound_statements,
        r.total_statements,
        pct(r.bound_statements, r.total_statements)
    );
    println!("diagnostic_counts: {:?}", r.diagnostic_counts);
    let mut bucket_categories: HashMap<String, usize> = HashMap::new();
    for (_, reason) in &r.buckets {
        let category = reason.split(':').next().unwrap_or(reason).to_string();
        *bucket_categories.entry(category).or_insert(0) += 1;
    }
    println!("bucket_categories: {bucket_categories:?}");
    println!();
}

fn pct(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        100.0 * n as f64 / d as f64
    }
}

fn write_buckets(output_dir: &Path, name: &str, r: &CorpusResult) {
    let _ = fs::create_dir_all(output_dir);
    let path = output_dir.join(format!("{name}_buckets.txt"));
    let body: String = r
        .buckets
        .iter()
        .map(|(f, reason)| format!("{f}\t{reason}\n"))
        .collect();
    let _ = fs::write(path, body);
}

fn main() {
    let admin_dir = std::env::var("MYBATIS_ADMIN_DIR").expect("set MYBATIS_ADMIN_DIR");
    let api_dir = std::env::var("MYBATIS_API_DIR").expect("set MYBATIS_API_DIR");
    let batch_dir = std::env::var("IBATIS_BATCH_DIR").expect("set IBATIS_BATCH_DIR");
    let output_dir = std::env::var("K1_OUTPUT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("batis-xml-k1"));

    let mybatis = measure_mybatis(&[
        ("admin", PathBuf::from(&admin_dir)),
        ("api", PathBuf::from(&api_dir)),
    ]);
    print_summary("mybatis (admin+api)", &mybatis);
    write_buckets(&output_dir, "mybatis", &mybatis);

    let ibatis = measure_ibatis(&PathBuf::from(&batch_dir));
    print_summary("ibatis (batch)", &ibatis);
    write_buckets(&output_dir, "ibatis", &ibatis);

    println!(
        "Per-file failure buckets written to {}",
        output_dir.display()
    );
}
