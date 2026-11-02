// SPDX-License-Identifier: GPL-3.0-or-later
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, readdir, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { IMAGE, releaseVersion, compareVersions, validateReceipt, generateCatalog } from '../scripts/ha-release.mjs';
const sha = 'a'.repeat(40), digest = `sha256:${'1'.repeat(64)}`;
const receipt = { version: '0.6.6', sha, image: `${IMAGE}:0.6.6`, digest,
  testedPlatforms: ['linux/amd64', 'linux/arm64'], platformDigests: { 'linux/amd64': `sha256:${'2'.repeat(64)}`, 'linux/arm64': `sha256:${'3'.repeat(64)}` } };
const manifest = { manifests: Object.entries(receipt.platformDigests).map(([platform, digest]) => {
  const [os, architecture] = platform.split('/'); return { platform: { os, architecture }, digest };
}) };
test('only exact stable version tags are accepted', () => {
  assert.equal(releaseVersion('v0.6.6', '0.6.6'), '0.6.6');
  for (const tag of ['store-v0.6.6', 'v0.6.6-beta.1', 'v0.6.7', 'v00.6.6', 'v0.6.6\n']) assert.throws(() => releaseVersion(tag, '0.6.6'));
  assert.equal(compareVersions('0.10.0', '0.9.9'), 1);
  assert.equal(compareVersions('0.6.5', '0.6.6'), -1);
  assert.equal(compareVersions('0.6.6', '0.6.6'), 0);
});
test('publication requires both tested architectures from the released commit and exact image digests', () => {
  validateReceipt(receipt, '0.6.6', sha, manifest);
  for (const patch of [{ sha: 'b'.repeat(40) }, { version: '0.6.7' }, { testedPlatforms: ['linux/amd64'] }, { image: `${IMAGE}:latest` }, { platformDigests: {} }]) {
    assert.throws(() => validateReceipt({ ...receipt, ...patch }, '0.6.6', sha, manifest));
  }
  assert.throws(() => validateReceipt(receipt, '0.6.6', sha, { manifests: manifest.manifests.slice(0, 1) }));
});
test('catalog contains only distribution files and pins the exact tested version', async () => {
  const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
  const directory = await mkdtemp(path.join(os.tmpdir(), 'mbf-ha-catalog-'));
  try {
    await generateCatalog(root, directory, '0.6.6', receipt);
    assert.deepEqual((await readdir(directory)).sort(), ['README.md', 'mybrewfolio_sync', 'release.json', 'repository.json']);
    const config = JSON.parse(await readFile(path.join(directory, 'mybrewfolio_sync/config.json')));
    assert.equal(config.version, '0.6.6'); assert.equal(config.image, IMAGE);
    assert.deepEqual(config.arch, ['amd64', 'aarch64']); assert.equal(config.panel_admin, true);
    assert.equal(config.backup, 'cold'); assert.equal(config.ingress, true);
    for (const key of ['ports', 'host_network', 'privileged', 'docker_api', 'hassio_api', 'homeassistant_api', 'map']) assert.equal(config[key], undefined);
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test('catalog CLI rejects a downgrade or changed release before modifying the destination', async () => {
  const { writeFile } = await import('node:fs/promises');
  const { execFile } = await import('node:child_process');
  const { promisify } = await import('node:util');
  const exec = promisify(execFile);
  const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
  const directory = await mkdtemp(path.join(os.tmpdir(), 'mbf-ha-release-'));
  const destination = path.join(directory, 'catalog');
  const currentVersion = JSON.parse(await readFile(path.join(root, 'package.json'))).version;
  const currentReceipt = { ...receipt, version: currentVersion, image: `${IMAGE}:${currentVersion}` };
  try {
    const files = ['receipt.json', 'manifest.json', 'previous.json'].map(name => path.join(directory, name));
    await writeFile(files[0], JSON.stringify(currentReceipt));
    await writeFile(files[1], JSON.stringify(manifest));
    for (const previous of [{ ...currentReceipt, version: '9999.0.0' }, { ...currentReceipt, sha: 'b'.repeat(40) }, { ...currentReceipt, digest: `sha256:${'4'.repeat(64)}` }]) {
      await writeFile(files[2], JSON.stringify(previous));
      await assert.rejects(exec(process.execPath, [path.join(root, 'scripts/ha-release.mjs'), 'catalog', `v${currentVersion}`, sha, files[0], files[1], destination, files[2]]));
      await assert.rejects(readdir(destination), { code: 'ENOENT' });
    }
    await writeFile(files[2], JSON.stringify(currentReceipt));
    await exec(process.execPath, [path.join(root, 'scripts/ha-release.mjs'), 'catalog', `v${currentVersion}`, sha, files[0], files[1], destination, files[2]]);
    assert.deepEqual(JSON.parse(await readFile(path.join(destination, 'release.json'))), currentReceipt);
  } finally { await rm(directory, { recursive: true, force: true }); }
});
