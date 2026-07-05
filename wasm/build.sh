#!/usr/bin/env bash
# One-command build for the batis-xml npm package: wasm-pack build, ship
# the (committed, drift-checked) TypeScript types alongside it, and patch
# package.json to the npm name (batis-xml differs from the Cargo package
# name batis-xml-wasm -- see DEVELOPMENT.md). Not published from here.
#
# Usage: ./wasm/build.sh
set -euo pipefail
cd "$(dirname "$0")/.."

wasm-pack build wasm --target nodejs

cp wasm/schema.d.ts wasm/pkg/schema.d.ts
cp LICENSE-MIT LICENSE-APACHE wasm/pkg/

node - <<'NODE'
const fs = require("fs");

const pkgPath = "wasm/pkg/package.json";
const pkg = JSON.parse(fs.readFileSync(pkgPath, "utf8"));
pkg.name = "batis-xml";
for (const f of ["schema.d.ts", "LICENSE-MIT", "LICENSE-APACHE"]) {
  if (!pkg.files.includes(f)) {
    pkg.files.push(f);
  }
}
// "." is the normal JS+types entry point; "./schema" is a types-only
// subpath (no runtime file -- schema.d.ts is ambient declarations) for
// `import type { ParseResult } from "batis-xml/schema"`.
pkg.exports = {
  ".": {
    types: "./batis_xml_wasm.d.ts",
    default: "./batis_xml_wasm.js",
  },
  "./schema": {
    types: "./schema.d.ts",
  },
};
fs.writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + "\n");
NODE

echo "wasm/pkg ready: $(node -p "require('./wasm/pkg/package.json').name")@$(node -p "require('./wasm/pkg/package.json').version")"
