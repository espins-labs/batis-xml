//! P0 permanent regression: dynamic-tag / resultMap nesting used to cost
//! one native Rust call-stack frame per level, through the mutual
//! recursion in `flatten.rs` (`flatten_segments` <-> its `expand_*`
//! family, and separately `union_walk` <-> itself) and, independently,
//! `parse.rs`'s `collect_mappings` <-> itself.
//!
//! **Root cause** (measured 2026-07-11, reproduced against this crate's
//! own pre-fix source in a throwaway `/tmp` copy -- never against this
//! repo's own git state -- via a scratch probe harness, since deleted,
//! that this test supersedes): each of those frames was large enough (`Ctx`/diagnostics
//! threaded by `&mut`, several `String`/`Vec` locals per `expand_*` call,
//! `Alt`/`Piece` accumulators) that `DEPTH_LIMIT` (256) levels of nesting
//! overflowed a 64 KiB thread stack in both debug and release, and even a
//! 1 MiB thread in debug -- on a Windows-default 1 MiB main thread, a
//! debug build could `STATUS_STACK_OVERFLOW`/abort (uncatchable, not even
//! a panic) on realistic, not pathological, dynamic SQL well under
//! `DEPTH_LIMIT` levels of nesting, violating this crate's "no panics on
//! public paths" contract (`CLAUDE.md` rule 4). Existing coverage
//! (`deeply_nested_if_tags_in_a_statement_returns_normally_with_diagnostic`/
//! `deeply_nested_association_in_a_result_map_returns_normally_with_diagnostic`
//! in `src/parse.rs`'s own test module) exercised 3000-level nesting but
//! only ever ran on the default test-harness thread, which happened to
//! have enough stack to mask the defect -- this test is the one that
//! actually pins a small-stack budget.
//!
//! **Fix**: both recursive families now run on a `Vec`-backed heap
//! worklist instead of the real call stack (`flatten.rs`'s
//! `Frame`/`BodyFrame`/`ChooseFrame` + `UFrame`/`UnionFrame`/
//! `UnionChooseFrame`; `parse.rs`'s `MFrame`/`MappingFrame`/
//! `DiscriminatorFrame`) -- nesting depth now costs one frame on a `Vec`'s
//! heap allocation per level, not one native call-stack frame. See
//! `flatten.rs`'s own module-level doc comment for the full design (ported
//! from the sibling `beans-xml` crate's `depth_engine`/
//! `dispatch::BeansBodyFrame` modules, which hit the identical defect
//! class first).
//!
//! **Both-directions verification**: this exact set of fixtures/depths/
//! stack budget was run against this crate's pre-fix source (a `cp -R`
//! throwaway copy in `/tmp`, `git checkout --` there only -- this repo's
//! own git state was never touched) and reliably aborted the process
//! (`SIGABRT`/stack overflow) at the hostile depth on a 64 KiB thread in
//! *both* debug and release; the post-fix source in *this* tree passes it
//! in both profiles too. 64 KiB (not the crate's nominal 256 KiB
//! acceptance bar) is the budget actually used below precisely because it
//! is the largest round size confirmed to still discriminate pre-fix from
//! post-fix in *both* profiles -- at 256 KiB, pre-fix release does not
//! overflow (only debug does), so a 256 KiB budget would make this test
//! vacuous in release. Re-run that comparison by hand if this test's own
//! history is ever in doubt: copy the crate to a scratch directory,
//! `git checkout <pre-fix commit> -- src/flatten.rs src/parse.rs` there,
//! and run this same fixture shape on a 64 KiB thread in both `cargo test`
//! and `cargo test --release`.
//!
//! `DEPTH_LIMIT` (256, `pub(crate)` in `flatten.rs`, not re-exported) is
//! hardcoded here rather than imported -- same convention the
//! (since-deleted) scratch probe harness used, and required by this
//! crate's own "public API unchanged" constraint (adding a new `pub`
//! export purely for a test would itself be a public-API change).
const DEPTH_LIMIT: usize = 256;
const HOSTILE_DEPTH: usize = DEPTH_LIMIT + 20;
const VALID_DEPTH: usize = DEPTH_LIMIT - 1;
/// Comfortably below a Windows main thread's 1 MiB default, and small
/// enough to also stand in for a small thread-pool worker, musl, or wasm --
/// clears this crate's own 256 KiB acceptance bar with a 4x margin. Chosen
/// over the nominal 256 KiB itself (cold-review finding, both directions
/// re-verified 2026-07-11): the post-fix engines pass all four fixtures at
/// 64 KiB in *both* debug and release, while the pre-fix recursive code
/// still reliably overflows at 64 KiB in *both* profiles too -- unlike
/// 256 KiB, where pre-fix release happens to survive (only pre-fix debug
/// overflows there), which would make a 256 KiB budget discriminate
/// pre/post-fix in debug only.
#[cfg(not(windows))]
const SMALL_STACK_BYTES: usize = 64 * 1024;
/// Windows exception: 64 KiB is below MSVC debug's *fixed* cost of merely
/// entering the parse pipeline on a fresh thread (fatter debug frames +
/// thread startup overhead) -- the depth-independent floor, not the
/// per-level growth this test pins, overflowed on windows-latest CI at
/// 64 KiB. 128 KiB clears that floor while still discriminating
/// pre/post-fix in debug (pre-fix debug needed >1 MiB); release
/// discrimination is retained by the Unix budget above.
#[cfg(windows)]
const SMALL_STACK_BYTES: usize = 128 * 1024;

/// One deeply-nested fixture: `name` for failure messages, `source` the
/// mapper XML, `expects_nesting_diag` whether a `NestingLimitExceeded`
/// diagnostic is the *correct* outcome at this depth (hostile) or would be
/// a regression (valid/`deep_generic`, which never enters either
/// depth-limited engine at all -- see below).
struct Fixture {
    name: &'static str,
    source: String,
    expects_nesting_diag: bool,
}

/// `<if>` nesting -- exercises `flatten.rs`'s cartesian engine
/// (`BodyFrame`), which bails into the union engine (`UnionFrame`) well
/// before `depth` levels via `BranchLimitExceeded` (each `<if>` doubles the
/// branch count) -- so this fixture's deep nesting is actually walked by
/// *both* engines in sequence, exercising both.
fn deep_if(depth: usize) -> String {
    let mut body = String::from("x = 1");
    for _ in 0..depth {
        body = format!(r#"<if test="a">{body}</if>"#);
    }
    format!(r#"<mapper namespace="x"><select id="s">SELECT 1{body}</select></mapper>"#)
}

/// `<choose>/<when>` nesting -- a different recursion arm through the
/// cartesian engine (`ChooseFrame`, not just `BodyFrame`), also expected to
/// fall through to the union engine at this depth.
fn deep_choose(depth: usize) -> String {
    let mut body = String::from("x = 1");
    for _ in 0..depth {
        body = format!(r#"<choose><when test="a">{body}</when></choose>"#);
    }
    format!(r#"<mapper namespace="x"><select id="s">SELECT 1{body}</select></mapper>"#)
}

/// `<resultMap>` `<association>` nesting -- exercises `parse.rs`'s
/// `collect_mappings` engine (`MappingFrame`), entirely separate from
/// `flatten.rs`'s two engines.
fn deep_assoc(depth: usize) -> String {
    let mut body = String::from(r#"<result column="c" property="p"/>"#);
    for _ in 0..depth {
        body = format!(r#"<association property="a">{body}</association>"#);
    }
    format!(r#"<mapper namespace="x"><resultMap id="rm" type="T">{body}</resultMap></mapper>"#)
}

/// Deeply nested *unrecognized top-level* elements, direct children of
/// `<mapper>` -- never inside a statement/fragment body or a `<resultMap>`,
/// so this never enters either depth-limited engine at all; only the raw
/// XML tree build (`skip_subtree`'s iterative counter, not real recursion)
/// walks it. Proven healthy already (measured 2026-07-11: passes a 64 KiB
/// thread at this same depth even pre-fix) -- included here as a control:
/// this axis needed no code change, and must keep passing with *no*
/// `NestingLimitExceeded` at any depth, unlike the other three fixtures.
fn deep_generic(depth: usize) -> String {
    format!(
        "<mapper namespace=\"x\">{}{}</mapper>",
        "<foo>".repeat(depth),
        "</foo>".repeat(depth)
    )
}

fn hostile_fixtures() -> Vec<Fixture> {
    vec![
        Fixture {
            name: "deep_if",
            source: deep_if(HOSTILE_DEPTH),
            expects_nesting_diag: true,
        },
        Fixture {
            name: "deep_choose",
            source: deep_choose(HOSTILE_DEPTH),
            expects_nesting_diag: true,
        },
        Fixture {
            name: "deep_assoc",
            source: deep_assoc(HOSTILE_DEPTH),
            expects_nesting_diag: true,
        },
        Fixture {
            name: "deep_generic",
            source: deep_generic(HOSTILE_DEPTH),
            expects_nesting_diag: false,
        },
    ]
}

fn valid_fixtures() -> Vec<Fixture> {
    vec![
        Fixture {
            name: "deep_if",
            source: deep_if(VALID_DEPTH),
            expects_nesting_diag: false,
        },
        Fixture {
            name: "deep_choose",
            source: deep_choose(VALID_DEPTH),
            expects_nesting_diag: false,
        },
        Fixture {
            name: "deep_assoc",
            source: deep_assoc(VALID_DEPTH),
            expects_nesting_diag: false,
        },
        Fixture {
            name: "deep_generic",
            source: deep_generic(VALID_DEPTH),
            expects_nesting_diag: false,
        },
    ]
}

/// Runs every hostile and valid fixture, on one 64 KiB thread, inside a
/// single test -- mirrors the sibling `beans-xml` crate's
/// `i3_p0_deep_semantic_recursion_small_stack_does_not_overflow` (same
/// fixture/depth/stack-budget shape, adapted to this crate's own dynamic-
/// SQL/resultMap recursion instead of bean/collection nesting). Runnable in
/// both debug and release (`cargo test` / `cargo test --release`) -- this
/// defect reproduced in both profiles, so both must be covered.
#[test]
fn small_stack_deep_nesting_does_not_overflow() {
    let hostile = hostile_fixtures();
    let valid = valid_fixtures();

    let handle = std::thread::Builder::new()
        .name("batis-xml-small-stack-regression".to_string())
        .stack_size(SMALL_STACK_BYTES)
        .spawn(move || {
            for fixture in &hostile {
                let result = batis_xml::parse(&fixture.source);
                let has_nesting_diag = result
                    .diagnostics
                    .iter()
                    .any(|d| d.code == batis_xml::DiagCode::NestingLimitExceeded);
                assert_eq!(
                    has_nesting_diag, fixture.expects_nesting_diag,
                    "{} (hostile, {HOSTILE_DEPTH} levels) NestingLimitExceeded presence \
                     mismatch: expected {}, got {:?}",
                    fixture.name, fixture.expects_nesting_diag, result.diagnostics
                );
                // Every hostile fixture must still return a mapper (the
                // over-limit subtree is dropped, not the whole document --
                // same "opaque subtree, not a hard failure" contract every
                // other anomaly in this crate follows).
                assert!(
                    result.mapper.is_some(),
                    "{} (hostile) must still produce a Mapper",
                    fixture.name
                );
            }
            for fixture in &valid {
                let result = batis_xml::parse(&fixture.source);
                let has_nesting_diag = result
                    .diagnostics
                    .iter()
                    .any(|d| d.code == batis_xml::DiagCode::NestingLimitExceeded);
                assert_eq!(
                    has_nesting_diag, fixture.expects_nesting_diag,
                    "{} at depth-(DEPTH_LIMIT - 1) (maximum legal depth) NestingLimitExceeded \
                     presence mismatch: expected {}, got {:?}",
                    fixture.name, fixture.expects_nesting_diag, result.diagnostics
                );
                let mapper = result
                    .mapper
                    .unwrap_or_else(|| panic!("{} (valid) must produce a Mapper", fixture.name));
                // deep_generic's own body is entirely unrecognized
                // top-level elements (never a statement/fragment/
                // resultMap -- see its own doc comment), so it never
                // populates `statements`/`result_maps` at any depth; the
                // other three fixtures must.
                if fixture.name != "deep_generic" {
                    assert!(
                        !mapper.statements.is_empty() || !mapper.result_maps.is_empty(),
                        "{} (valid) must produce a fully-parsed statement or resultMap, not an \
                         empty/opaque stub",
                        fixture.name
                    );
                }
            }
        })
        .expect("spawning a 64 KiB thread should succeed");

    handle.join().expect(
        "64 KiB thread must not panic/overflow while parsing every deep_* hostile fixture and \
         every depth-(DEPTH_LIMIT - 1) valid fixture",
    );
}

/// Spot-check well past `HOSTILE_DEPTH`: stack use must not grow with
/// nesting depth at all now (the whole point of the heap-worklist
/// conversion), not just barely clear the documented threshold -- proves
/// this on the same 64 KiB budget at ~18x `DEPTH_LIMIT`.
#[test]
fn small_stack_extreme_depth_5000_does_not_overflow() {
    const EXTREME_DEPTH: usize = 5000;
    let fixtures = [
        ("deep_if", deep_if(EXTREME_DEPTH)),
        ("deep_choose", deep_choose(EXTREME_DEPTH)),
        ("deep_assoc", deep_assoc(EXTREME_DEPTH)),
        ("deep_generic", deep_generic(EXTREME_DEPTH)),
    ];

    let handle = std::thread::Builder::new()
        .name("batis-xml-small-stack-extreme-depth".to_string())
        .stack_size(SMALL_STACK_BYTES)
        .spawn(move || {
            for (name, source) in &fixtures {
                let result = batis_xml::parse(source);
                assert!(
                    result.mapper.is_some(),
                    "{name} at depth {EXTREME_DEPTH} must still produce a Mapper"
                );
            }
        })
        .expect("spawning a 64 KiB thread should succeed");

    handle
        .join()
        .expect("64 KiB thread must not panic/overflow at extreme (5000-level) nesting depth");
}
