// SPDX-License-Identifier: GPL-3.0-or-later
import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { resolve } from 'node:path';
import sharp from 'sharp';
const root = resolve(import.meta.dirname, '..');
for (const name of ['textlogosync.svg', 'app-icon.svg', 'tray-template.svg', 'tray-color.svg']) {
  const svg = await readFile(resolve(root, 'assets', name), 'utf8');
  assert(!/<(?:image|text)\b|base64,/i.test(svg), `${name} must be pure vector`);
}
const { data, info } = await sharp(resolve(root, 'src-tauri/icons/tray-template.png')).ensureAlpha().raw().toBuffer({ resolveWithObject: true });
assert.equal(info.width, 36);
assert.equal(info.height, 36);
let clear = 0, ink = 0;
for (let i = 0; i < data.length; i += 4) {
  if (data[i + 3] === 0) clear++;
  else {
    ink++;
    assert.equal(data[i] + data[i + 1] + data[i + 2], 0, 'macOS template must use only black and clear pixels');
  }
}
assert(clear > 0 && ink > 0);
for (const [name, size] of [['assets/textlogosync-1024.png', 1024], ['assets/mybrewfolio-sync-app-v2-1024.png', 1024],
  ['assets/microsoft-store/MyBrewFolio-Sync-72.png', 72], ['assets/microsoft-store/MyBrewFolio-Sync-150.png', 150],
  ['assets/microsoft-store/MyBrewFolio-Sync-300.png', 300], ['src-tauri/icons/tray-color.png', 36]]) {
  const image = sharp(resolve(root, name));
  const metadata = await image.metadata();
  assert.equal(metadata.width, size, name);
  assert.equal(metadata.height, size, name);
  const pixel = await image.ensureAlpha().extract({ left: 0, top: 0, width: 1, height: 1 }).raw().toBuffer();
  assert.deepEqual([...pixel], [224, 112, 53, 255], `${name} must use the approved background`);
}
for (const name of await readdir(resolve(root, 'windows/Assets'))) {
  const metadata = await sharp(resolve(root, 'windows/Assets', name)).metadata();
  const base = name.startsWith('StoreLogo') ? [50, 50] : name.startsWith('Square44') ? [44, 44] : name.startsWith('Square150') ? [150, 150] : [310, 150];
  const scale = Number(/scale-(\d+)/.exec(name)?.[1] || 100) / 100;
  const target = /targetsize-(\d+)/.exec(name)?.[1];
  assert.equal(metadata.width, target ? Number(target) : Math.round(base[0] * scale), name);
  assert.equal(metadata.height, target ? Number(target) : Math.round(base[1] * scale), name);
}
const manifest = await readFile(resolve(root, 'windows/Package.appxmanifest.xml'), 'utf8');
for (const match of manifest.matchAll(/Assets\\([^"<>]+\.png)/g)) {
  await readFile(resolve(root, 'windows/Assets', match[1]));
}
assert(manifest.includes('BackgroundColor="#E07035"'));
const ico = await readFile(resolve(root, 'src-tauri/icons/icon.ico'));
assert.equal(ico.readUInt16LE(2), 1);
const sizes = Array.from({ length: ico.readUInt16LE(4) }, (_, index) => ico[6 + index * 16] || 256);
for (const size of [16, 24, 32, 48, 64, 256]) assert(sizes.includes(size));
const icns = await readFile(resolve(root, 'src-tauri/icons/icon.icns'));
assert.equal(icns.toString('ascii', 0, 4), 'icns');
assert.equal(icns.readUInt32BE(4), icns.length);
console.log('Native icon formats, MSIX DPI sizes, colors and macOS template verified.');
