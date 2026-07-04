//! Prints, for one or more mapper XML files, every statement's and
//! fragment's `<include>` edges as an indented text tree -- the include
//! chain (MM-04/MM-05) resolved as far as this single mapper allows.
//! Cross-namespace and `${}`-driven refids are marked unresolved rather
//! than guessed at.
//!
//! Run with: cargo run --example include_graph -- path/to/Mapper.xml [more paths...]

use batis_xml::{IncludeRef, IncludeTarget, Mapper, Spanned, SqlFragment};
use std::collections::HashMap;
use std::env;
use std::fs;

fn fragment_map(mapper: &Mapper) -> HashMap<&str, &SqlFragment> {
    mapper
        .fragments
        .iter()
        .map(|f| (f.id.value.as_str(), f))
        .collect()
}

fn print_includes(
    includes: &[Spanned<IncludeRef>],
    fragments: &HashMap<&str, &SqlFragment>,
    depth: usize,
    seen: &mut Vec<String>,
) {
    let indent = "  ".repeat(depth);
    for inc in includes {
        match &inc.value.target {
            IncludeTarget::Local(id) => {
                println!("{indent}-> {id}");
                if seen.contains(id) {
                    println!("{indent}   (cycle, stopping)");
                    continue;
                }
                match fragments.get(id.as_str()) {
                    Some(fragment) => {
                        seen.push(id.clone());
                        print_includes(&fragment.includes, fragments, depth + 1, seen);
                        seen.pop();
                    }
                    None => {
                        println!("{indent}   (unresolved: no <sql id=\"{id}\"> in this mapper)")
                    }
                }
            }
            IncludeTarget::Qualified { ns, id } => {
                println!("{indent}-> {ns}.{id} (external namespace, not resolved)");
            }
            IncludeTarget::Dynamic => {
                println!("{indent}-> <dynamic refid, unresolvable statically>");
            }
        }
    }
}

fn main() {
    let paths: Vec<String> = env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: include_graph <mapper.xml> [more paths...]");
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
        let fragments = fragment_map(mapper);

        for stmt in &mapper.statements {
            let id = stmt
                .id
                .as_ref()
                .map(|s| s.value.as_str())
                .unwrap_or("<missing id>");
            println!("statement {id}");
            if stmt.includes.is_empty() {
                println!("  (no includes)");
            } else {
                print_includes(&stmt.includes, &fragments, 1, &mut Vec::new());
            }
        }

        for fragment in &mapper.fragments {
            println!("fragment {}", fragment.id.value);
            if fragment.includes.is_empty() {
                println!("  (no includes)");
            } else {
                let mut seen = vec![fragment.id.value.clone()];
                print_includes(&fragment.includes, &fragments, 1, &mut seen);
            }
        }
    }
}
