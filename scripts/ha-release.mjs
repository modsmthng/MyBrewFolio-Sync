// SPDX-License-Identifier: GPL-3.0-or-later
// Pure validation is also exercised without GitHub credentials in CI.
import { cp, mkdir, readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

export const IMAGE = 'ghcr.io/modsmthng/mybrewfolio-sync-ha';
export const PLATFORMS = ['linux/amd64', 'linux/arm64'];
export function releaseVersion(tag, packageVersion) {
  const match = /^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.exec(tag);
  if (!match || tag.slice(1) !== packageVersion) throw new Error('Stable release tag and package version must match');
  return tag.slice(1);
}
export function compareVersions(a, b) {
  const parse = value => {
    if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(value)) throw new Error('Invalid catalog version');
    return value.split('.').map(BigInt);
  };
  const aa = parse(a), bb = parse(b);
  for (let index = 0; index < 3; index++) if (aa[index] !== bb[index]) return aa[index] > bb[index] ? 1 : -1;
  return 0;
}
export function validateReceipt(receipt, version, sha, manifest) {
  if (receipt.version !== version || receipt.sha !== sha || receipt.image !== `${IMAGE}:${version}`
    || !/^sha256:[a-f0-9]{64}$/.test(receipt.digest)) throw new Error('Build receipt does not match the release');
  if (!PLATFORMS.every(platform => receipt.testedPlatforms?.includes(platform))) throw new Error('Both architectures must have passed the container test');
  const platforms = manifest.manifests?.map(item => `${item.platform.os}/${item.platform.architecture}`) || [];
  if (!PLATFORMS.every(platform => platforms.includes(platform))) throw new Error('Multi-architecture image is incomplete');
  for (const platform of PLATFORMS) {
    const digest = receipt.platformDigests?.[platform];
    if (!/^sha256:[a-f0-9]{64}$/.test(digest || '') || !manifest.manifests.some(item => `${item.platform.os}/${item.platform.architecture}` === platform && item.digest === digest)) throw new Error('Image architecture differs from the tested image');
  }
}
export async function generateCatalog(source, destination, version, receipt) {
  await mkdir(destination, { recursive: true });
  for (const name of ['repository.json', 'README.md', 'mybrewfolio_sync']) {
    await cp(path.join(source, 'home-assistant', name), path.join(destination, name), { recursive: true });
  }
  const configPath = path.join(destination, 'mybrewfolio_sync/config.json');
  const config = JSON.parse(await readFile(configPath, 'utf8'));
  config.version = version;
  await writeFile(configPath, JSON.stringify(config, null, 2) + '\n');
  await writeFile(path.join(destination, 'release.json'), JSON.stringify(receipt, null, 2) + '\n');
}

async function main() {
  const [command, ...args] = process.argv.slice(2);
  const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
  const pkg = JSON.parse(await readFile(path.join(root, 'package.json'), 'utf8'));
  if (command === 'version') {
    const version = releaseVersion(args[0] || `v${pkg.version}`, pkg.version);
    const cargo = await readFile(path.join(root, 'src-tauri/Cargo.toml'), 'utf8');
    const tauri = JSON.parse(await readFile(path.join(root, 'src-tauri/tauri.conf.json'), 'utf8'));
    const config = JSON.parse(await readFile(path.join(root, 'home-assistant/mybrewfolio_sync/config.json'), 'utf8'));
    if (cargo.match(/^version = "([^"]+)"/m)?.[1] !== version || tauri.version !== version || config.version !== version) throw new Error('Release versions differ');
    console.log(version);
  } else if (command === 'test-evidence') {
    const [platform, sha, digest, destination] = args;
    if (!PLATFORMS.includes(platform) || !/^[a-f0-9]{40}$/.test(sha) || !/^sha256:[a-f0-9]{64}$/.test(digest)) throw new Error('Invalid tested-image identity');
    await writeFile(destination, JSON.stringify({ platform, sha, passed: true, digest }) + '\n');
  } else if (command === 'receipt') {
    const [version, sha, digest, destination, ...files] = args;
    releaseVersion(`v${version}`, pkg.version);
    const tests = await Promise.all(files.map(file => readFile(file, 'utf8').then(JSON.parse)));
    if (!PLATFORMS.every(platform => tests.some(test => test.platform === platform && test.sha === sha && test.passed === true))) throw new Error('Missing successful architecture tests for this commit');
    const receipt = { version, sha, image: `${IMAGE}:${version}`, digest, testedPlatforms: PLATFORMS, platformDigests: Object.fromEntries(PLATFORMS.map(platform => [platform, tests.find(test => test.platform === platform).digest])) };
    if (!/^sha256:[a-f0-9]{64}$/.test(digest)) throw new Error('Invalid image digest');
    await writeFile(destination, JSON.stringify(receipt, null, 2) + '\n');
  } else if (command === 'catalog') {
    const [tag, sha, receiptPath, manifestPath, destination, previousPath] = args;
    const version = releaseVersion(tag, pkg.version);
    const receipt = JSON.parse(await readFile(receiptPath, 'utf8'));
    validateReceipt(receipt, version, sha, JSON.parse(await readFile(manifestPath, 'utf8')));
    if (previousPath) {
      const previous = JSON.parse(await readFile(previousPath, 'utf8'));
      const order = compareVersions(version, previous.version);
      if (order < 0) throw new Error('Refusing to replace the catalog with an older release');
      if (order === 0 && (previous.digest !== receipt.digest || previous.sha !== receipt.sha)) throw new Error('Refusing to replace an immutable release');
    }
    await generateCatalog(root, destination, version, receipt);
  } else {
    throw new Error('Usage: ha-release.mjs version [tag] | receipt VERSION SHA DIGEST OUTPUT TEST... | catalog TAG SHA RECEIPT MANIFEST OUTPUT [PREVIOUS]');
  }
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
