// Smoke test for the batis-xml-wasm bindings (nodejs target).
//
// Not part of `cargo test` -- this checks the built wasm-pack output, so
// it must run *after* `wasm-pack build wasm --target nodejs`:
//
//   wasm-pack build wasm --target nodejs
//   node wasm/tests/smoke.js
//
// Parses one fixture XML (fixtures/mybatis/dynamic_column_marker.xml,
// chosen because it's the one fixture exercising the ${} -> __BATIS_DYN__
// substitution) and asserts a known statement id and the marker both
// appear in the returned JSON.

const fs = require("fs");
const path = require("path");
const wasm = require("../pkg/batis_xml_wasm.js");

const fixturePath = path.join(
  __dirname,
  "..",
  "..",
  "fixtures",
  "mybatis",
  "dynamic_column_marker.xml",
);
const bytes = fs.readFileSync(fixturePath);

const json = wasm.parse(new Uint8Array(bytes));
const result = JSON.parse(json);

assert(
  json.includes("searchWidgetsBySortColumn"),
  "expected statement id 'searchWidgetsBySortColumn' in the JSON output",
);
assert(
  json.includes("__BATIS_DYN__"),
  "expected the __BATIS_DYN__ marker in the JSON output",
);
assert(
  result.mapper.statements[0].id.value === "searchWidgetsBySortColumn",
  "expected the parsed statement id to round-trip through JSON.parse",
);
assert(
  typeof result.mapper.statements[0].span === "object" &&
    typeof result.mapper.statements[0].span.start === "number",
  "expected the statement's span field to be present",
);

const detected = wasm.detect(new Uint8Array(bytes));
assert(
  detected === "mybatis",
  `expected detect() to return the plain string 'mybatis', got ${JSON.stringify(detected)}`,
);
assert(
  detected === result.dialect,
  "expected detect() to agree with parse()'s dialect field",
);

console.log(`wasm.version() = ${wasm.version()}`);
console.log(`wasm.detect() = ${detected}`);
console.log(`JSON output size: ${json.length} bytes`);
console.log(
  "PASS: statement id, span, __BATIS_DYN__ marker, and detect() all present/correct",
);

function assert(cond, message) {
  if (!cond) {
    console.error(`FAIL: ${message}`);
    process.exit(1);
  }
}
