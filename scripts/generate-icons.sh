#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later

set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_dir=$(dirname -- "$script_dir")
tray_source=$(mktemp "${TMPDIR:-/tmp}/mybrewfolio-tray-source.XXXXXX")

cleanup() {
  rm -f "$tray_source"
}
trap cleanup EXIT INT TERM

cd "$repo_dir"

npm exec tauri icon -- \
  assets/textlogosync-1024.png \
  --output src-tauri/icons

node - "$tray_source" <<'NODE'
const fs = require("node:fs");

const output = process.argv[2];
const svg = fs.readFileSync("assets/tray-template.svg", "utf8");
const match = /base64,\s*([^"]+)/s.exec(svg);
if (!match) {
  throw new Error("assets/tray-template.svg does not contain an embedded PNG");
}
fs.writeFileSync(output, Buffer.from(match[1].replaceAll(/\s/g, ""), "base64"));
NODE

if ! command -v sips >/dev/null 2>&1; then
  echo "sips is required to render the macOS tray template" >&2
  exit 1
fi
sips -z 36 36 "$tray_source" --out src-tauri/icons/tray-template.png >/dev/null

node <<'NODE'
const fs = require("node:fs");

const expected = new Map([
  ["assets/microsoft-store/MyBrewFolio-Sync-72.png", 72],
  ["assets/microsoft-store/MyBrewFolio-Sync-150.png", 150],
  ["assets/microsoft-store/MyBrewFolio-Sync-300.png", 300],
  ["src-tauri/icons/tray-template.png", 36],
]);

for (const [filename, size] of expected) {
  const bytes = fs.readFileSync(filename);
  const signature = bytes.subarray(1, 4).toString("ascii");
  const width = bytes.readUInt32BE(16);
  const height = bytes.readUInt32BE(20);
  if (signature !== "PNG" || width !== size || height !== size) {
    throw new Error(`${filename} must be an exact ${size}x${size} PNG`);
  }
}

console.log("Native, tray, and Microsoft Store icon assets verified.");
NODE
