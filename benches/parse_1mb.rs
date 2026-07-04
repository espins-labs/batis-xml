//! Spec DoD: parse a 1 MB mapper in < 50 ms (criterion, local baseline).
//!
//! The input is generated deterministically (no `proptest` randomness) so
//! the benchmark is reproducible run to run. It's structurally similar to
//! the MM-12 property generator — repeated statements with nested
//! `<if>`/`<choose>`/`<foreach>` and `#{}` placeholders — but is a fixed
//! string, not committed to the repo (regenerated on every `cargo bench`).

use batis_xml::parse;
use criterion::{black_box, criterion_group, criterion_main, Criterion};

const STATEMENT_TEMPLATE: &str = r#"<select id="stmt_{i}">
    SELECT a, b, c FROM widget
    <if test="name != null">AND name = #{{name}}</if>
    <choose>
        <when test="status == 'A'">AND status = 'A'</when>
        <when test="status == 'B'">AND status = 'B'</when>
        <otherwise>AND status IS NOT NULL</otherwise>
    </choose>
    <if test="tags != null">
        <foreach item="tag" collection="tags" open="AND tag IN (" separator="," close=")">
            #{{tag}}
        </foreach>
    </if>
</select>"#;

/// Builds a MyBatis mapper of at least `target_bytes` bytes by repeating a
/// fixed statement shape with an incrementing id.
fn generate_mapper(target_bytes: usize) -> String {
    let mut out = String::from(r#"<mapper namespace="com.example.bench.Gen">"#);
    let mut i = 0usize;
    while out.len() < target_bytes {
        out.push_str(&STATEMENT_TEMPLATE.replace("{i}", &i.to_string()));
        i += 1;
    }
    out.push_str("</mapper>");
    out
}

fn bench_parse_1mb(c: &mut Criterion) {
    let source = generate_mapper(1_000_000);
    eprintln!(
        "parse_1mb: generated mapper is {} bytes, {} statements",
        source.len(),
        source.matches("<select ").count()
    );

    let mut group = c.benchmark_group("parse_1mb_mapper");
    group.bench_function("parse", |b| {
        b.iter(|| black_box(parse(black_box(&source))));
    });
    group.finish();
}

criterion_group!(benches, bench_parse_1mb);
criterion_main!(benches);
