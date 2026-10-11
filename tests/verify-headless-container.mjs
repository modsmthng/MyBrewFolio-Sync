// SPDX-License-Identifier: GPL-3.0-or-later
// Run against a locally built image. All containers are isolated from the network;
// the test creates and removes only its own uniquely named volumes and containers.
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';
import { promisify } from 'node:util';

const exec = promisify(execFile);
const image = process.argv[2] || 'mybrewfolio-sync:local-control';
const prefix = `mybrewfolio-sync-smoke-${randomUUID()}`;
const volumes = [`${prefix}-data`, `${prefix}-secret`, `${prefix}-nas`, `${prefix}-bind-fixtures`];
const containers = new Set();
const docker = async (...args) => (await exec('docker', args, { timeout: 60_000 })).stdout.trim();

async function start(name, volume, extra = []) {
  return startWithMount(name, `type=volume,source=${volume},target=/data`, extra);
}
async function startWithMount(name, mount, extra = []) {
  containers.add(name);
  await docker('run', '-d', '--name', name, '--network', 'none',
    '--mount', mount, ...extra, image);
  for (let attempt = 0; attempt < 30; attempt++) {
    try {
      const health = JSON.parse(await docker('exec', name, 'mybrewfolio-syncd', 'health'));
      assert.equal(health.ok, true);
      // health can fall back to a one-shot CLI before the socket is ready. Require
      // the daemon's socket as well so every subsequent command reaches the daemon.
      await docker('exec', name, 'test', '-S', '/data/control.sock');
      return;
    } catch {
      await delay(200);
    }
  }
  throw new Error(`Container ${name} did not become ready`);
}
async function remove(name) {
  await docker('rm', '-f', name);
  containers.delete(name);
}
async function keyMetadata(name, path = '/data/state.key') {
  return docker('exec', name, 'stat', '-c', '%s %a %u %g', path);
}

async function storageFailure(name, mount) {
  containers.add(name);
  await docker('run', '-d', '--name', name, '--network', 'none', '--mount', mount, image);
  assert.equal(await docker('wait', name), '78');
  const { stdout, stderr } = await exec('docker', ['logs', name], { timeout: 60_000 });
  const logs = stdout + stderr;
  assert.match(logs, /Cannot write to Sync data directory \/data \(UID 10001, GID 10001\)/);
  assert.match(logs, /user: "UID:GID"/);
  assert.match(logs, /read\/write/);
  assert.doesNotMatch(logs, /Connect MyBrewFolio:|\/account\/sync/);
  await remove(name);
  return logs;
}

try {
  for (const volume of volumes) await docker('volume', 'create', volume);
  const first = `${prefix}-first`;
  await start(first, volumes[0], ['-e', 'MYBREWFOLIO_SYNC_GAGGIMATE_HOST=192.168.42.1']);
  assert.equal(await docker('exec', first, 'id', '-u'), '10001');
  assert.equal(await keyMetadata(first), '32 600 10001 10001');
  const keyDigest = (await docker('exec', first, 'sha256sum', '/data/state.key')).split(' ')[0];
  await docker('exec', first, 'mybrewfolio-syncd', 'host', 'set', '192.168.42.2');
  await remove(first);

  const recreated = `${prefix}-recreated`;
  await start(recreated, volumes[0]);
  assert.equal((await docker('exec', recreated, 'sha256sum', '/data/state.key')).split(' ')[0], keyDigest);
  const status = JSON.parse(await docker('exec', recreated, 'mybrewfolio-syncd', 'status'));
  assert.equal(status.machineHost, '192.168.42.2');
  await remove(recreated);

  // Move only this test's key to a separate volume and verify the legacy key-file
  // override works without creating a replacement in /data.
  await docker('run', '--rm', '--network', 'none', '--user', '0:0', '--entrypoint', 'sh',
    '-v', `${volumes[0]}:/data`, '-v', `${volumes[1]}:/secrets`, image,
    '-c', 'cp -p /data/state.key /secrets/original.key && rm /data/state.key');
  const external = `${prefix}-external`;
  await start(external, volumes[0], ['--mount', `type=volume,source=${volumes[1]},target=/secrets,readonly`,
    '-e', 'MYBREWFOLIO_SYNC_CREDENTIAL_KEY_FILE=/secrets/original.key']);
  await docker('exec', external, 'test', '!', '-e', '/data/state.key');
  assert.equal((await docker('exec', external, 'sha256sum', '/secrets/original.key')).split(' ')[0], keyDigest);
  await remove(external);

  // Model a NAS appdata folder owned by Unraid's nobody/users identity.
  await docker('run', '--rm', '--network', 'none', '--user', '0:0', '--entrypoint', 'chown',
    '-v', `${volumes[2]}:/data`, image, '99:100', '/data');
  const nas = `${prefix}-nas`;
  await start(nas, volumes[2], ['--user', '99:100']);
  assert.equal(await keyMetadata(nas), '32 600 99 100');
  await remove(nas);

  // Bind directories on the daemon's Linux filesystem. Docker Desktop's host
  // file-sharing permission emulation can otherwise hide a Linux ownership error.
  // A disposable fixture volume gives us a private daemon-local parent to clean up.
  const bindRoot = await docker('volume', 'inspect', '--format', '{{.Mountpoint}}', volumes[3]);
  await docker('run', '--rm', '--network', 'none', '--user', '0:0', '--entrypoint', 'mkdir',
    '--mount', `type=volume,source=${volumes[3]},target=/fixture`, image,
    '/fixture/writable', '/fixture/denied');
  const writable = `${bindRoot}/writable`;
  const denied = `${bindRoot}/denied`;
  const bindMount = path => `type=bind,source=${path},target=/data`;
  await docker('run', '--rm', '--network', 'none', '--user', '0:0', '--entrypoint', 'chown',
    '--mount', bindMount(writable), image, '10001:10001', '/data');
  await docker('run', '--rm', '--network', 'none', '--user', '0:0', '--entrypoint', 'chmod',
    '--mount', bindMount(writable), image, '700', '/data');
  const bound = `${prefix}-bound`;
  await startWithMount(bound, bindMount(writable));
  assert.equal(await keyMetadata(bound), '32 600 10001 10001');
  const bindDigest = (await docker('exec', bound, 'sha256sum', '/data/state.key')).split(' ')[0];
  await remove(bound);

  await docker('run', '--rm', '--network', 'none', '--user', '0:0', '--entrypoint', 'chown',
    '--mount', bindMount(denied), image, '0:0', '/data');
  await docker('run', '--rm', '--network', 'none', '--user', '0:0', '--entrypoint', 'chmod',
    '--mount', bindMount(denied), image, '700', '/data');
  const deniedLogs = await storageFailure(`${prefix}-denied`, bindMount(denied));
  assert.match(deniedLogs, /Permission denied/);
  // The failed preflight must not create SQLite or account state.
  await docker('run', '--rm', '--network', 'none', '--user', '0:0', '--entrypoint', 'test',
    '--mount', bindMount(denied), image, '!', '-e', '/data/sync.sqlite');
  const readonlyLogs = await storageFailure(`${prefix}-readonly`, `${bindMount(writable)},readonly`);
  assert.match(readonlyLogs, /Read-only file system/);
  const rebound = `${prefix}-rebound`;
  await startWithMount(rebound, bindMount(writable));
  assert.equal((await docker('exec', rebound, 'sha256sum', '/data/state.key')).split(' ')[0], bindDigest);
  await remove(rebound);
  console.log('Container verified: non-root startup, persistent state, external key, NAS UID/GID, writable/denied/read-only bind mounts.');
} finally {
  for (const name of containers) await docker('rm', '-f', name);
  for (const volume of volumes) await docker('volume', 'rm', volume);
}
