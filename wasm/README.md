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

## Three things that will bite you

**(a) Feed raw bytes — never a host-pre-decoded string.** Always pass the
file's original `Buffer`/`Uint8Array` to `parse`/`detect`, not a string
you already decoded (e.g. `fs.readFileSync(path, "utf-8")` then
re-encoded). `batis-xml` detects the encoding itself: UTF-8 first, then
BOM/declared-label-driven (all WHATWG encodings via `encoding_rs` —
Shift_JIS, GB18030, Big5, UTF-16, …), with an EUC-KR heuristic for
declaration-less legacy files; anything else decodes lossily with a
diagnostic. Feeding it bytes that already went through a host UTF-8
decoder defeats all of that, since a genuinely non-UTF-8 file would
already have been mangled (replacement characters) before `batis-xml`
ever sees it. Read files as bytes and stay in bytes until you call in.

**(b) Spans are UTF-8 byte offsets — not JS string indices.** Every
`ByteSpan { start, end }` in the JSON indexes into UTF-8 *bytes* of the
source (see `ByteSpan`'s own doc in `schema.d.ts` for the EUC-KR
re-encoding caveat), while a JS string is indexed by UTF-16 code units.
These silently diverge the moment a multi-byte character appears before
the offset you care about. To slice correctly:

```js
// bytes is the same Buffer/Uint8Array you fed to parse()
const text = Buffer.from(bytes).subarray(span.start, span.end).toString("utf-8");
```

If you only have a JS string (already decoded), re-encode it to UTF-8
bytes first (`Buffer.from(str, "utf-8")`) before slicing by these
offsets — don't use the string's own `.slice()`/`.substring()` with span
offsets.

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

## One more thing

`IncludeRef.raw` (the raw, unparsed `refid` text) is kept even when
`IncludeTarget` is `Dynamic` (a `${}`-driven refid `batis-xml` can't
resolve statically) — inspect it for a static prefix or pattern to
attempt your own best-effort match, rather than treating every dynamic
include as a dead end.
