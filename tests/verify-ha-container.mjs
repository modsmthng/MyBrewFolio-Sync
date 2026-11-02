// SPDX-License-Identifier: GPL-3.0-or-later
// A disposable internal network simulates Supervisor's real TCP source address.
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { randomUUID } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';
const exec = promisify(execFile);
const image = process.argv[2] || 'mybrewfolio-sync-ha:test';
const prefix = `mbf-ha-${randomUUID()}`;
const network = `${prefix}-network`;
const volumes = [`${prefix}-data`, `${prefix}-backup`];
const containers = new Set();
const docker = async (...args) => (await exec('docker', args, { timeout: 60_000, maxBuffer: 2 ** 20 })).stdout.trim();
const helper = 'node:24-bookworm-slim';
async function request(container, path, { trusted = true, method = 'GET', body, headers = {} } = {}) {
  const script = `const r=await fetch(process.argv[1],JSON.parse(process.argv[2]));console.log(JSON.stringify({status:r.status,headers:Object.fromEntries(r.headers),body:await r.text()}));`;
  return JSON.parse(await docker('run', '--rm', '--network', network, '--ip', trusted ? '172.30.32.2' : '172.30.32.3', helper,
    'node', '--input-type=module', '-e', script, `http://${container}:8099/${path}`,
    JSON.stringify({ method, headers, body: body === undefined ? undefined : JSON.stringify(body) })));
}
async function start(name, volume) {
  containers.add(name);
  await docker('run', '-d', '--name', name, '--network', network, '--ip', '172.30.32.10', '-v', `${volume}:/data`, image);
  for (let attempt = 0; attempt < 60; attempt++) {
    try {
      await docker('exec', name, 'test', '-S', '/data/state/control.sock');
      assert.equal(JSON.parse(await docker('exec', name, 'mybrewfolio-syncd', 'health')).ok, true);
      return;
    } catch { await delay(500); }
  }
  throw new Error('HA daemon did not become ready');
}
async function remove(name) { await docker('rm', '-f', name); containers.delete(name); }
try {
  await docker('pull', helper);
  await docker('network', 'create', '--internal', '--subnet', '172.30.32.0/24', network);
  for (const volume of volumes) await docker('volume', 'create', volume);
  await docker('run', '--rm', '--network', 'none', '-v', `${volumes[0]}:/data`, '--entrypoint', 'sh', image,
    '-c', 'printf \'{"unrelated":"preserve"}\\n\' > /data/options.json');
  const first = `${prefix}-first`;
  await start(first, volumes[0]);
  const processStatus = await docker('exec', first, 'cat', '/proc/1/status');
  assert.match(processStatus, /Uid:\s+10001\s+10001\s+10001\s+10001/);
  assert.deepEqual(JSON.parse(await docker('exec', first, 'cat', '/data/options.json')), { unrelated: 'preserve' });
  assert.equal(await docker('exec', first, 'stat', '-c', '%s %a %u %g', '/data/state/state.key'), '32 600 10001 10001');
  const keyDigest = (await docker('exec', first, 'sha256sum', '/data/state/state.key')).split(' ')[0];
  const denied = await request(first, 'api/status', { trusted: false, headers: { 'X-Forwarded-For': '172.30.32.2', 'X-Remote-User-Id': 'admin' } });
  assert.equal(denied.status, 403);
  const snapshot = await request(first, 'api/status');
  assert.equal(snapshot.status, 200);
  assert.equal(snapshot.headers['cache-control'], 'no-store');
  const data = JSON.parse(snapshot.body);
  assert.equal(data.status.connected, false);
  assert.deepEqual(data.status.machines, []);
  assert.equal(typeof data.csrfToken, 'string');
  assert.doesNotMatch(snapshot.body, /refresh_token|access_token|codeVerifier|pollToken/);
  const home = await request(first, '');
  assert.equal(home.status, 200);
  const assets = [...home.body.matchAll(/(?:src|href)="([^"]+\.(?:js|css))"/g)].map(match => match[1]);
  assert.ok(assets.length >= 2);
  for (const asset of assets) {
    assert.ok(!asset.startsWith('/'), 'Ingress assets must be relative');
    assert.equal((await request(first, asset.replace(/^\.\//, ''))).status, 200);
  }
  assert.equal((await request(first, 'api/action', { method: 'POST', body: { action: 'disconnect', confirm: true }, headers: { 'Content-Type': 'application/json' } })).status, 403);
  assert.equal((await request(first, 'api/action', { method: 'POST', body: { action: 'disconnect', confirm: false }, headers: { 'Content-Type': 'application/json', 'X-MyBrewFolio-CSRF': data.csrfToken } })).status, 400);
  await docker('exec', first, 'mybrewfolio-syncd', 'host', 'set', '192.168.42.2');
  // Cold backup and replacement/update preserve the entire private state directory.
  await remove(first);
  await docker('run', '--rm', '--network', 'none', '-v', `${volumes[0]}:/original:ro`, '-v', `${volumes[1]}:/backup`, '--entrypoint', 'sh', image,
    '-c', 'cp -a /original/. /backup/');
  const restored = `${prefix}-restored`;
  await start(restored, volumes[1]);
  assert.equal((await docker('exec', restored, 'sha256sum', '/data/state/state.key')).split(' ')[0], keyDigest);
  const after = JSON.parse((await request(restored, 'api/status')).body);
  assert.equal(after.status.machineHost, '192.168.42.2');
  assert.notEqual(after.csrfToken, data.csrfToken, 'Stale browser mutations must be rejected after replacement');
  assert.equal((await request(restored, 'api/action', { method: 'POST', body: { action: 'connect' }, headers: { 'Content-Type': 'application/json', 'X-MyBrewFolio-CSRF': data.csrfToken } })).status, 403);
  console.log('HA container verified: non-root service, ingress isolation, relative assets, CSRF, cold backup and persistent state.');
} finally {
  for (const name of containers) await docker('rm', '-f', name).catch(() => {});
  for (const volume of volumes) await docker('volume', 'rm', volume).catch(() => {});
  await docker('network', 'rm', network).catch(() => {});
}
