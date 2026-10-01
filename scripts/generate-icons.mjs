// SPDX-License-Identifier: GPL-3.0-or-later
import assert from 'node:assert/strict';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { execFileSync } from 'node:child_process';
import sharp from 'sharp';

const root = resolve(import.meta.dirname, '..');
const source = async name => {
  const svg = await readFile(resolve(root, 'assets', name));
  assert(!/<(?:image|text)\b|base64,/i.test(svg.toString()), `${name} must be pure vector`);
  return svg;
};
const logo = (await source('textlogosync.svg')).toString();
const template = await source('tray-template.svg');
function colored(svg, size, scale = 1) {
  return Buffer.from(svg.replaceAll('fill="#000000"', 'fill="#FBFBFB"')
    .replace(/(<svg[^>]*>)/, `$1<rect width="${size}" height="${size}" fill="#E07035"/><g transform="translate(${size * (1 - scale) / 2} ${size * (1 - scale) / 2}) scale(${scale})">`)
    .replace('</svg>', '</g></svg>'));
}
const app = colored(logo, 1024, 0.86);
const tray = colored(template.toString(), 512);
await writeFile(resolve(root, 'assets/app-icon.svg'), app);
await writeFile(resolve(root, 'assets/tray-color.svg'), tray);
const output = resolve(root, 'src-tauri/icons');
const msix = resolve(root, 'windows/Assets');
await mkdir(msix, { recursive: true });
await mkdir(resolve(root, 'assets/microsoft-store'), { recursive: true });
// The locked Tauri CLI supplies the ICNS, ICO, desktop and mobile PNG families.
execFileSync(process.execPath, [resolve(root, 'node_modules/@tauri-apps/cli/tauri.js'), 'icon',
  'assets/app-icon.svg', '--output', 'src-tauri/icons', '--ios-color', '#E07035'], { cwd: root, stdio: 'inherit' });
// Tauri emits ICNS chunks from a hash map; normalize their order for byte-stable exports.
const icnsPath = resolve(output, 'icon.icns');
const icns = await readFile(icnsPath);
const chunks = [];
for (let offset = 8; offset < icns.length;) {
  const length = icns.readUInt32BE(offset + 4);
  chunks.push(icns.subarray(offset, offset + length));
  offset += length;
}
chunks.sort((a, b) => Buffer.compare(a.subarray(0, 4), b.subarray(0, 4)));
await writeFile(icnsPath, Buffer.concat([icns.subarray(0, 8), ...chunks]));

async function png(svg, width, height, path) {
  await sharp(svg, { density: 384 }).resize(width, height, { fit: 'contain', background: '#E07035' })
    .ensureAlpha().png().toFile(resolve(root, path));
}
await png(app, 1024, 1024, 'assets/textlogosync-1024.png');
await png(app, 1024, 1024, 'assets/mybrewfolio-sync-app-v2-1024.png');
for (const size of [18, 36]) {
  // Resize keeps clear pixels outside the black template, without a colored canvas.
  await sharp(template, { density: 384 }).resize(size, size).ensureAlpha().png()
    .toFile(resolve(output, size === 36 ? 'tray-template.png' : 'tray-template-18.png'));
}
for (const size of [16, 24, 32, 48, 64]) await png(tray, size, size, `src-tauri/icons/tray-color-${size}.png`);
await png(tray, 36, 36, 'src-tauri/icons/tray-color.png');
for (const size of [72, 150, 300]) await png(app, size, size, `assets/microsoft-store/MyBrewFolio-Sync-${size}.png`);
const tiles = [['StoreLogo', 50, 50], ['Square44x44Logo', 44, 44],
  ['Square150x150Logo', 150, 150], ['Wide310x150Logo', 310, 150]];
for (const [name, width, height] of tiles) {
  await png(app, width, height, `windows/Assets/${name}.png`);
  for (const scale of [100, 125, 150, 200, 400]) {
    await png(app, Math.round(width * scale / 100), Math.round(height * scale / 100), `windows/Assets/${name}.scale-${scale}.png`);
  }
}
for (const size of [16, 24, 32, 48, 256]) {
  await png(app, size, size, `windows/Assets/Square44x44Logo.targetsize-${size}.png`);
}
console.log('Generated native, tray, MSIX and Microsoft Store icons from vector masters.');

await import('./verify-icons.mjs');
