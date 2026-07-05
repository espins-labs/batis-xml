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
const vm = require("node:vm");
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

// B39 (cold code review, minor): `instanceof Uint8Array` is realm-bound --
// a genuine Uint8Array constructed via node:vm's separate context (a
// stand-in for a different iframe/Worker realm) has a *different*
// Uint8Array constructor identity, so `instanceof` alone would wrongly
// reject it. parse()/detect() must accept it via the duck-typed fallback.
const otherRealmUint8Array = vm.runInContext(
  "Uint8Array",
  vm.createContext({}),
);
const crossRealmBytes = new otherRealmUint8Array(bytes);
assert(
  !(crossRealmBytes instanceof Uint8Array),
  "test setup: this array must actually be from a different realm",
);
const crossRealmResult = JSON.parse(wasm.parse(crossRealmBytes));
assert(
  crossRealmResult.mapper.statements[0].id.value ===
    "searchWidgetsBySortColumn",
  "expected a cross-realm Uint8Array to parse the same as a same-realm one",
);
assert(
  wasm.detect(crossRealmBytes) === "mybatis",
  "expected detect() to also accept a cross-realm Uint8Array",
);
console.log("PASS: parse()/detect() accept a cross-realm Uint8Array");

// A detached backing buffer (e.g. already transferred to a Worker via
// postMessage) must give a friendly, specific message -- not whatever raw
// exception the JS engine's Uint8Array constructor happens to throw.
if (typeof ArrayBuffer.prototype.transfer === "function") {
  const detachedSource = new otherRealmUint8Array(bytes);
  detachedSource.buffer.transfer();
  try {
    wasm.parse(detachedSource);
    assert(false, "expected parse(detached) to throw");
  } catch (err) {
    assert(
      err instanceof TypeError,
      `expected a TypeError for a detached buffer, got ${err}`,
    );
    assert(
      err.message.includes("detached"),
      `expected the detached-buffer message to say so plainly, got: ${err.message}`,
    );
    // Not just "mentions detached" (the raw V8 exception -- "Cannot
    // perform %TypedArray%.prototype.set on a detached ArrayBuffer" --
    // also does) but this crate's own actionable wording, so this
    // assertion actually distinguishes the friendly message from the
    // engine's raw one (see the B42 same-realm case below, which
    // regressed to the raw message without this distinction).
    assert(
      err.message.includes("Pass a live Uint8Array/Buffer instead"),
      `expected the crate's specific actionable wording, not the raw engine error, got: ${err.message}`,
    );
  }
  console.log("PASS: parse() gives a friendly message for a detached buffer");
} else {
  console.log(
    "SKIP: ArrayBuffer.prototype.transfer unavailable in this Node version",
  );
}

// B42 (cold code review, major): the *same-realm* fast path (a genuine
// `instanceof Uint8Array` that passes the dyn_ref check directly) used to
// skip the friendly-message mapping entirely and call `arr.to_vec()`
// straight away, which throws the JS engine's raw
// "%TypedArray%.prototype.set on a detached ArrayBuffer" error instead of
// this crate's specific, actionable message -- only the cross-realm
// duck-type path above got the friendly wording. Both realms must now
// give the identical friendly message.
if (typeof ArrayBuffer.prototype.transfer === "function") {
  const sameRealmDetached = new Uint8Array(bytes);
  sameRealmDetached.buffer.transfer();
  assert(
    sameRealmDetached instanceof Uint8Array,
    "test setup: this array must be a genuine same-realm Uint8Array",
  );
  try {
    wasm.parse(sameRealmDetached);
    assert(false, "expected parse(same-realm detached) to throw");
  } catch (err) {
    assert(
      err instanceof TypeError,
      `expected a TypeError for a same-realm detached buffer, got ${err}`,
    );
    assert(
      err.message.includes("detached"),
      `expected the same-realm detached-buffer message to say so plainly, got: ${err.message}`,
    );
    // This is the assertion that actually catches B42: before the fix,
    // the same-realm fast path threw the raw V8 exception ("Cannot
    // perform %TypedArray%.prototype.set on a detached ArrayBuffer"),
    // which also happens to contain the substring "detached" -- so the
    // check above alone does not distinguish the bug from the fix. Only
    // the crate's own specific wording does.
    assert(
      err.message.includes("Pass a live Uint8Array/Buffer instead"),
      `expected the crate's specific actionable wording (same as the cross-realm case), not the raw engine error, got: ${err.message}`,
    );
  }
  console.log(
    "PASS: parse() gives the same friendly message for a SAME-realm detached buffer",
  );
} else {
  console.log(
    "SKIP: ArrayBuffer.prototype.transfer unavailable in this Node version",
  );
}

// A plain Array must still be rejected (no BYTES_PER_ELEMENT at all) --
// the duck-typing fallback must not loosen A16's original guarantee.
assertThrowsTypeError(() => wasm.parse([1, 2, 3]), "parse", "a plain array");
console.log("PASS: parse() still rejects a plain Array");

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
