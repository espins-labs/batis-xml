//! WebAssembly bindings for `batis-xml`. Minimal surface, JSON-string
//! boundary: consumers get the whole output model (schema v1, see
//! `../schema/batis-xml.v1.json`) as a JSON string rather than a
//! marshalled JS object -- simplest, schema-faithful, no per-field glue
//! code to keep in sync as the model grows.

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// A16 (cold code review, major): `&[u8]`'s wasm-bindgen marshalling
/// coerces *any* JS value into bytes rather than validating it's really a
/// `Uint8Array`/`Buffer` first -- a JS string silently became a
/// zero-filled buffer (parsing it then failed with a misleading "no root
/// element found", not an error pointing at the real mistake), a number
/// "succeeded" with equally meaningless output, and `null`/`undefined`
/// threw an internal `TypeError` from deep inside the generated glue code
/// instead of a clear message. Accepting `&JsValue` and validating with a
/// real `instanceof Uint8Array` check (via `JsCast::dyn_ref`, which also
/// accepts Node's `Buffer` -- a `Uint8Array` subclass) turns all of these
/// into one explicit, actionable `TypeError`.
fn require_bytes(input: &JsValue, fn_name: &str) -> Result<Vec<u8>, JsValue> {
    // B42 (cold code review, major): this used to fast-path same-realm
    // `Uint8Array`/`Buffer` inputs straight through `arr.to_vec()`, which
    // internally calls `%TypedArray%.prototype.set` on the underlying
    // `ArrayBuffer` -- for a *detached* buffer (e.g. already transferred
    // to a Worker via `postMessage`, same realm or not) that throws the
    // raw, unhelpful "%TypedArray%.prototype.set on a detached
    // ArrayBuffer" engine error, while the cross-realm duck-type path
    // below already mapped the identical failure mode to a friendly,
    // actionable message. Same failure, two different error shapes
    // depending purely on which realm the input happened to come from.
    //
    // Fix: both realms went through the same fallible copy path
    // (`copy_bytes_from`, `Reflect`-based so it's `catch`-enabled rather
    // than trapping the wasm instance) and the same friendly-message
    // mapping on failure -- there's a single code path for "read the
    // bytes out of a byte-shaped input", not two.
    //
    // B48 (cold code review, minor): that unification made every
    // same-realm call pay *two* copies -- `copy_bytes_from` builds a
    // fresh `Uint8Array` via `Reflect.construct` (one copy) and then
    // `.to_vec()`s it (a second copy) even when `input` already *is* a
    // live, same-realm `Uint8Array` that could have been `.to_vec()`d
    // directly. Measured +50% on copy-dominated `detect()` for 5 MB
    // input.
    //
    // Fix: a same-realm `Uint8Array` with `byte_length() > 0` is
    // guaranteed live -- a *detached* view's `byteLength` reads back as
    // `0` per the spec (detaching zeroes the view's length/byteLength
    // rather than leaving stale values), so a non-zero `byteLength` here
    // proves the buffer has not been detached and `.to_vec()` cannot
    // trap. Take that single-copy path directly. A `byteLength` of `0`
    // is ambiguous between "genuinely empty" and "detached", so it still
    // routes through the fallible `copy_bytes_from` path below, which
    // tells the two apart by whether the `Reflect.construct` copy
    // actually throws.
    if let Some(arr) = input.dyn_ref::<js_sys::Uint8Array>() {
        if arr.byte_length() > 0 {
            return Ok(arr.to_vec());
        }
    }

    if input.dyn_ref::<js_sys::Uint8Array>().is_some() || looks_like_a_byte_typed_array(input) {
        return copy_bytes_from(input).map_err(|_| {
            js_sys::TypeError::new(&format!(
                "{fn_name}() was given a byte array whose contents could not be read -- \
                 its underlying buffer is likely detached (e.g. already transferred to \
                 a Worker via postMessage). Pass a live Uint8Array/Buffer instead."
            ))
            .into()
        });
    }

    Err(js_sys::TypeError::new(&format!(
        "{fn_name}() expects the raw file bytes as a Uint8Array/Buffer -- got {}. \
         Do not pass a decoded string (see README: feed raw bytes, not a \
         host-pre-decoded string) -- read the file as bytes instead.",
        describe_js_value(input)
    ))
    .into())
}

/// Copies bytes out of a byte-shaped input (same-realm `Uint8Array`/
/// `Buffer`, or a cross-realm duck-typed equivalent) via a fresh,
/// same-realm `Uint8Array` built through `Reflect.construct` -- see
/// [`construct_uint8_array_from`]. Used for *both* realms (B42): a
/// detached backing `ArrayBuffer` makes the `new Uint8Array(x)` call
/// itself throw, surfaced here as `Err` rather than trapping the wasm
/// instance, regardless of which realm `input` was constructed in.
fn copy_bytes_from(input: &JsValue) -> Result<Vec<u8>, JsValue> {
    construct_uint8_array_from(input).map(|arr| arr.to_vec())
}

/// Duck-types "is this byte-shaped like a Uint8Array/Buffer" without
/// relying on `instanceof` (see [`require_bytes`]'s `B39` comment).
fn looks_like_a_byte_typed_array(input: &JsValue) -> bool {
    if !input.is_object() {
        return false;
    }
    let byte_length = js_sys::Reflect::get(input, &JsValue::from_str("byteLength"))
        .ok()
        .and_then(|v| v.as_f64());
    let bytes_per_element = js_sys::Reflect::get(input, &JsValue::from_str("BYTES_PER_ELEMENT"))
        .ok()
        .and_then(|v| v.as_f64());
    matches!((byte_length, bytes_per_element), (Some(_), Some(bpe)) if bpe == 1.0)
}

/// Builds a fresh, same-realm `Uint8Array` by calling the global
/// `Uint8Array` constructor via `Reflect.construct` (rather than
/// `js_sys::Uint8Array::new`, which isn't `catch`-enabled and would trap
/// the whole wasm instance instead of surfacing a `Result` if the
/// constructor throws -- see the detached-ArrayBuffer case above).
fn construct_uint8_array_from(input: &JsValue) -> Result<js_sys::Uint8Array, JsValue> {
    let global = js_sys::global();
    let ctor = js_sys::Reflect::get(&global, &JsValue::from_str("Uint8Array"))?;
    let ctor: js_sys::Function = ctor.unchecked_into();
    let args = js_sys::Array::of1(input);
    let value = js_sys::Reflect::construct(&ctor, &args)?;
    Ok(value.unchecked_into())
}

/// A short, human-readable description of a `JsValue`'s type for the error
/// message above -- not a full `typeof`, just enough to name the mistake.
fn describe_js_value(v: &JsValue) -> &'static str {
    if v.is_null() {
        "null"
    } else if v.is_undefined() {
        "undefined"
    } else if v.as_string().is_some() {
        "a string"
    } else if v.as_f64().is_some() {
        "a number"
    } else if v.as_bool().is_some() {
        "a boolean"
    } else if v.is_instance_of::<js_sys::Array>() {
        "a plain Array (not a Uint8Array)"
    } else {
        "an unsupported value"
    }
}

/// Parses mapper XML bytes and returns the `ParseResult` (schema v1) as a
/// JSON string. Never panics: encoding/parse failures already surface as
/// diagnostics inside the JSON per the core crate's contract, and the
/// (practically unreachable, since `ParseResult` has no non-string map
/// keys) serialization failure case falls back to the JSON literal `null`
/// rather than trapping the wasm instance. Throws a `TypeError` (rejecting
/// the call, not silently coercing) if `input` isn't a `Uint8Array`/`Buffer`
/// -- see [`require_bytes`].
#[wasm_bindgen]
pub fn parse(
    #[wasm_bindgen(unchecked_param_type = "Uint8Array")] input: &JsValue,
) -> Result<String, JsValue> {
    let bytes = require_bytes(input, "parse")?;
    let result = batis_xml::parse_bytes(&bytes);
    Ok(serde_json::to_string(&result).unwrap_or_else(|_| "null".to_string()))
}

/// Cheap dialect pre-check (MM-01 logic only, no statement/fragment/
/// resultMap capture or flattening) -- returns the plain string
/// (`"mybatis"` / `"ibatis"` / `"unknown"`, matching the schema's enum
/// spelling), not a JSON-quoted one: unlike `parse`, this returns a single
/// scalar with no nested model to keep schema-faithful, so there's no
/// reason to make callers `JSON.parse` it. Guaranteed to agree with
/// `parse(bytes)`'s `dialect` field (see the core crate's contract test).
/// Same `Uint8Array`/`Buffer` input validation as `parse` (A16).
///
/// B32 (cold code review, minor): typed as the `Dialect` union
/// (`unchecked_return_type`) rather than the generic `string` wasm-bindgen
/// would otherwise infer from `Result<String, _>` -- callers get real
/// autocomplete/type-narrowing on the three actual values instead of an
/// unconstrained string.
#[wasm_bindgen(unchecked_return_type = "\"mybatis\" | \"ibatis\" | \"unknown\"")]
pub fn detect(
    #[wasm_bindgen(unchecked_param_type = "Uint8Array")] input: &JsValue,
) -> Result<String, JsValue> {
    let bytes = require_bytes(input, "detect")?;
    let dialect = batis_xml::detect_dialect(&bytes);
    let json = serde_json::to_string(&dialect).unwrap_or_else(|_| "\"unknown\"".to_string());
    Ok(json.trim_matches('"').to_string())
}

/// This crate's version, from `Cargo.toml`.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
