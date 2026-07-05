# batis-xml

WebAssembly bindings for [`batis-xml`](https://github.com/espins-labs/batis-xml)
— a parser and dynamic-SQL flattener for MyBatis and iBatis mapper XML.

```js
const batisXml = require("batis-xml");

const bytes = fs.readFileSync("OrderMapper.xml"); // Buffer, NOT a decoded string
const result = JSON.parse(batisXml.parse(bytes)); // schema v1, see schema.d.ts
const dialect = batisXml.detect(bytes); // "mybatis" | "ibatis" | "unknown" -- cheap pre-check
```

TypeScript consumers: `schema.d.ts` ships in this package (generated from
`schema/batis-xml.v1.json`, drift-checked in CI) —
`import type { ParseResult } from "batis-xml/schema"`.

**Node.js target only — no browser/bundler build yet.** This package is
built with `wasm-pack --target nodejs` (CommonJS, loads the `.wasm` via
`fs.readFileSync` at require time). It will not work as-is in a browser
or with a bundler expecting `--target web`/`--target bundler` output
(`fetch`-based instantiation, ESM). That's a separate build target to
add later, not a difference in the Rust source.

## Four things that will bite you

**(a) Feed raw bytes — never a host-pre-decoded string.** Always pass the
file's original `Buffer`/`Uint8Array` to `parse`/`detect`, not a string
you already decoded (e.g. `fs.readFileSync(path, "utf-8")` then
re-encoded). `batis-xml` detects the encoding itself: a BOM sniff first
(UTF-16 LE/BE select directly; a UTF-8 BOM is stripped), then a UTF-8
attempt, then the XML declaration's own `encoding=` label
(BOM/declared-label-driven, covering every WHATWG encoding via
`encoding_rs` — Shift_JIS, GB18030, Big5, UTF-16, …), then an EUC-KR
heuristic for declaration-less legacy files; anything else decodes
lossily with a diagnostic. `result.encoding` reports which of these
actually won (see (b)). Feeding it bytes that already went through a host
UTF-8 decoder defeats all of that, since a genuinely non-UTF-8 file would
already have been mangled (replacement characters) before `batis-xml`
ever sees it. Read files as bytes and stay in bytes until you call in.

**(b) Spans are byte offsets into the UTF-8 text `batis-xml` itself
decoded — never JS string indices, and never the *original* file's raw
bytes for anything but a UTF-8 source.** Every `ByteSpan { start, end }`
in the JSON indexes into the UTF-8 bytes of the *decoded* text (see
`ByteSpan`'s own doc in `schema.d.ts`), while a JS string is indexed by
UTF-16 code units — these diverge the moment a multi-byte character
appears before the offset you care about. Worse, for anything decoded
from a non-UTF-8 encoding, the original file's raw bytes aren't even the
same *length* as the UTF-8 re-encoding the spans are offsets into — do
not slice the original `Buffer`/`Uint8Array` directly with these offsets.
`result.encoding` (the WHATWG name `TextDecoder` accepts directly) is
what makes this reproducible:

```js
// bytes is the same Buffer/Uint8Array you fed to parse()
const decodedText = new TextDecoder(result.encoding).decode(bytes);
const utf8Bytes = new TextEncoder().encode(decodedText); // byte-identical to batis-xml's own internal String
const text = new TextDecoder("utf-8").decode(
  utf8Bytes.subarray(span.start, span.end)
);
```

If the input was plain UTF-8, `bytes` and `utf8Bytes` are already
byte-identical (decoding then re-encoding UTF-8 is a no-op), so slicing
`bytes` directly happens to work in that one case — but relying on that
silently breaks the moment a file turns out to be Shift_JIS/EUC-KR/
UTF-16/etc., which is exactly the failure mode `result.encoding` exists
to prevent. Always go through the `TextDecoder`/`TextEncoder` round trip
above regardless of what encoding you expect.

**(c) Build qualified names as `ns.id`, suffixed `@databaseId` when
present.** `Statement.database_id` is deliberately *not* folded into
`id` — that's the consumer's call. If you build a "qualified name" key
(e.g. `namespace.statementId`) and drop `database_id`, two dual-dialect
variants of the same statement (`databaseId="oracle"` / `"mysql"`)
collide onto one key. Recommended recipe:

```js
const qualifiedName = statement.database_id
  ? `${namespace}.${statement.id.value}@${statement.database_id.value}`
  : `${namespace}.${statement.id.value}`;
```

**(d) `<include>` expands *before* `<where>`/`<set>`/`<trim>` in MyBatis/
iBatis — this crate doesn't expand it at all.** `IncludeRef` gives you the
raw `refid` plus a best-effort `IncludeTarget`; substituting the
referenced `<sql>` fragment's text in is on you.

The marker's textual form is a **stable v1 contract**: it renders in the
flattened SQL as the literal, fixed-prefix comment token
`` /* batis:include(<raw>) */ ``, where `<raw>` is `IncludeRef.raw`
verbatim (any literal `*/` inside it is replaced with `*_/` so it can't
terminate the comment early) — the same whether the target is `Local`,
`Qualified` (rendered with its original dot, e.g. `otherNs.frag`), or
`Dynamic` (the unresolved `${...}` text rendered as-is). Since the prefix
is fixed, `sqlText.indexOf("/* batis:include(")` (or a global regex) finds
every token directly; match each one to its `Statement.includes`/
`SqlFragment.includes` entry by reconstructing the exact token string
from that entry's `raw` field:

```js
for (const inc of statement.includes) {
  const token = `/* batis:include(${inc.value.raw.replaceAll("*/", "*_/")}) */`;
  sqlText = sqlText.replace(token, resolveFragmentText(inc.value)); // your own lookup
}
```

If the fragment you're substituting is itself multi-variant
(`sql.variants` on the fragment's own flattened output has more than one
entry), there's no single deterministic substitution — pick the
fragment's variant whose `conditions` match the same parameter state as
the *enclosing* statement's variant you're substituting into, not
`variants[0]` arbitrarily.

If you splice fragment text in *after* flattening (rather than before,
like the real engines do), redo the wrapper's own cleanup against that
substituted text: re-apply the leading-AND/OR strip or trailing-comma
strip when the include token was first/last inside the wrapper, and
treat a wrapper whose only content is an include token as conditional
(the fragment might expand to nothing). `result.diagnostics` carries
`include_at_wrapper_boundary` for every spot this applies to — each
diagnostic's `span` is the same original-XML span as the matching
`includes[]` entry's `span` (not a position in the flattened text) — see
the core crate's README ("Include expansion order") for the full
contract. As with any diagnostic, match on `code`
(`"include_at_wrapper_boundary"`), never on `message` text — messages
may be reworded between versions without that being a breaking change.

One more ordering guarantee worth relying on: `result.mapper.statements`
(and `fragments`/`result_maps`) preserve the source document's order.

## One more thing

`IncludeRef.raw` (the raw, unparsed `refid` text) is kept even when
`IncludeTarget` is `Dynamic` (a `${}`-driven refid `batis-xml` can't
resolve statically) — inspect it for a static prefix or pattern to
attempt your own best-effort match, rather than treating every dynamic
include as a dead end.
