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

node - <<'NODE'
const fs = require("fs");

const pkgPath = "wasm/pkg/package.json";
const pkg = JSON.parse(fs.readFileSync(pkgPath, "utf8"));
pkg.name = "batis-xml";
if (!pkg.files.includes("schema.d.ts")) {
  pkg.files.push("schema.d.ts");
}
fs.writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + "\n");
NODE

echo "wasm/pkg ready: $(node -p "require('./wasm/pkg/package.json').name")@$(node -p "require('./wasm/pkg/package.json').version")"
