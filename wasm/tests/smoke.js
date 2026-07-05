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

// A16 (cold code review, major): parse()/detect() used to silently coerce
// a wrong-typed input instead of rejecting it -- a string became a
// zero-filled buffer (misleading "no root element found", not an error
// about the real mistake), a number "succeeded" with meaningless output,
// and null/undefined threw an internal TypeError from deep inside the
// generated glue code. All three must now throw one clear, explicit
// TypeError instead.

assertThrowsTypeError(
  () => wasm.parse("<mapper></mapper>"),
  "parse",
  "a string",
);
assertThrowsTypeError(() => wasm.parse(42), "parse", "a number");
assertThrowsTypeError(() => wasm.parse(null), "parse", "null");
assertThrowsTypeError(() => wasm.detect("<mapper></mapper>"), "detect", "a string");
assertThrowsTypeError(() => wasm.detect(42), "detect", "a number");
assertThrowsTypeError(() => wasm.detect(null), "detect", "null");

console.log(
  "PASS: parse()/detect() reject string/number/null input with a clear TypeError",
);

function assert(cond, message) {
  if (!cond) {
    console.error(`FAIL: ${message}`);
    process.exit(1);
  }
}

function assertThrowsTypeError(fn, fnName, inputDescription) {
  try {
    fn();
  } catch (err) {
    assert(
      err instanceof TypeError,
      `expected ${fnName}(${inputDescription}) to throw a TypeError, got ${err}`,
    );
    assert(
      err.message.includes(fnName) && err.message.includes("Uint8Array"),
      `expected ${fnName}(${inputDescription})'s error message to name the function and expected type, got: ${err.message}`,
    );
    return;
  }
  assert(false, `expected ${fnName}(${inputDescription}) to throw, but it returned normally`);
}
