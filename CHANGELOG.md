# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/espins-labs/batis-xml/releases/tag/v0.1.0) - 2026-07-04

### Added

- generate TypeScript types + automate npm rename, add npm README
- add detect_dialect(bytes) core API + wasm detect() export
- add span field to Statement/SqlFragment/ResultMap
- add batis-xml-wasm bindings crate (workspace)
- add benchmark round-3 harness (examples/q_chain.rs)
- add dump_statements and include_graph DoD examples
- pin schema v1 via schemars behind an off-by-default feature
- *(mm-11)* iBatis <resultMap class=> + documented <parameterMap> gap
- *(mm-14)* EUC-KR/CP949 encoding detection
- *(mm-13)* hostile-input recovery — recoverable reader errors, oversize cap
- *(mm-09,mm-10)* class ref attributes + resultMap elements
- *(mm-06c)* iBatis conditional tags, <dynamic>, and <iterate>
- *(mm-06b)* <where>/<set>/<trim>/<foreach>/<bind> wrapper semantics
- *(mm-06a)* <if>/<choose> flattening, branch cap, and SqlText assembly
- *(mm-07)* placeholder normalization + property_paths
- *(mm-05)* <include refid> reference collection
- *(mm-04)* <sql id> fragment collection
- *(mm-08)* CDATA/entity handling via internal span-preserving segments
- *(mm-03)* statement element collection
- *(mm-02)* namespace attribute parsing
- *(mm-01)* root element detection + dialect determination

### Fixed

- disable git EOL conversion — byte spans require byte-exact checkouts
- *(mm-07)* don't pair unrelated bare #/$ delimiters across whitespace
- *(mm-03)* no silent breaks on reader errors + add Statement.database_id
- *(mm-02)* replace flat attribute scan with a proper tokenizer

### Other

- exclude batis-xml-wasm from crates.io releases (npm is its channel)
- add contribution policy and issue/PR templates
- add dynamic-column ${} marker fixture (mybatis)
- de-brand public strings, drop private-path references
- replace scaffold status with reality in README and lib.rs
- check off release wiring checklist item
- wire up release-plz and cargo-semver-checks
- check off conformance corpus checklist item
- expand conformance corpus to 18 MyBatis / 11 iBatis pairs
- check off schema + license pre-publish checklist items
- dual-license under MIT OR Apache-2.0
- mirror the ByteSpan re-encoding caveat into model.rs
- add criterion benchmark for 1 MB mapper parse
- enforce 90%+ line coverage gate via cargo-llvm-cov
- *(mm-12)* property-test suite over a synthetic mapper generator
- *(k1)* add K-1 corpus measurement harness + branch-explosion fixture
- *(conformance)* enable the first live conformance pairs
- point repository metadata at the espins-labs organization
- add git-safety rule (WIP commit before destructive commands)
- allow additive DiagCode variants with mandatory reporting
- pin toolchain via rust-toolchain.toml (stable + wasm32 target)
- batis-xml — MyBatis/iBatis mapper XML parser skeleton
