// SPDX-License-Identifier: GPL-3.0-or-later

import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { chmod, mkdtemp, mkdir, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repository = join(dirname(fileURLToPath(import.meta.url)), '..');
const installerPath = join(repository, 'scripts', 'install-headless.sh');

function run(command, arguments_, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, arguments_, { ...options, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', value => { stdout += value; });
    child.stderr.on('data', value => { stderr += value; });
    child.on('error', reject);
    child.on('close', code => resolve({ code, stdout, stderr }));
  });
}

const testRoot = await mkdtemp(join(tmpdir(), 'mybrewfolio-sync-installer-'));

try {
  const bin = join(testRoot, 'bin');
  const installDir = join(testRoot, 'installation');
  const dockerLog = join(testRoot, 'docker.log');
  await mkdir(bin);
  await writeFile(join(bin, 'docker'), `#!/usr/bin/env sh
set -eu
if [ "$1" = compose ] && [ "$2" = version ]; then
  printf '%s\\n' 'Docker Compose fixture'
  exit 0
fi
printf '%s\\n' "$*" >> "$MYBREWFOLIO_SYNC_TEST_DOCKER_LOG"
`);
  await writeFile(join(bin, 'openssl'), `#!/usr/bin/env sh
printf '%s\\n' 'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA='
`);
  await chmod(join(bin, 'docker'), 0o755);
  await chmod(join(bin, 'openssl'), 0o755);

  const environment = {
    ...process.env,
    PATH: `${bin}:${process.env.PATH}`,
    MYBREWFOLIO_SYNC_HOME: installDir,
    MYBREWFOLIO_SYNC_TEST_DOCKER_LOG: dockerLog
  };
  const nonInteractive = await run('sh', [installerPath, '--host', '192.168.1.42', '--non-interactive'], {
    cwd: repository,
    env: environment
  });
  assert.equal(nonInteractive.code, 0, nonInteractive.stderr);
  assert.match(nonInteractive.stdout, /Pair later with: .*sync auth begin/);
  assert.equal((await stat(join(installDir, 'sync'))).mode & 0o777, 0o700);
  assert.match(await readFile(join(installDir, '.env'), 'utf8'), /MYBREWFOLIO_SYNC_GAGGIMATE_HOST=192\.168\.1\.42/);
  assert.match(await readFile(join(installDir, 'compose.yaml'), 'utf8'), /mybrewfolio_sync_state_key/);

  const helper = await run(join(installDir, 'sync'), ['help'], { env: environment });
  assert.equal(helper.code, 0, helper.stderr);
  const dockerCalls = await readFile(dockerLog, 'utf8');
  assert.match(dockerCalls, /up -d/);
  assert.match(dockerCalls, /run --rm --no-deps sync help/);

  const missingHost = await run('sh', [installerPath, '--non-interactive'], {
    cwd: repository,
    env: { ...environment, MYBREWFOLIO_SYNC_HOME: join(testRoot, 'missing-host') }
  });
  assert.equal(missingHost.code, 64);
  assert.match(missingHost.stderr, /--host is required together with --non-interactive/);

  const installer = await readFile(installerPath, 'utf8');
  assert.match(installer, /GaggiMate host \[gaggimate\.local\]/);
  assert.match(installer, /Connect your MyBrewFolio account now/);
  assert.match(installer, /<\/dev\/tty/);
  assert.match(installer, /--non-interactive/);
  assert.match(installer, /HELPER_FILE/);
  assert.doesNotMatch(installer, /Connect your account once with/);

  const docs = await readFile(join(repository, 'docs', 'headless.md'), 'utf8');
  assert.ok(docs.indexOf('## Quick Start') < docs.indexOf('## Everyday Commands'));
  assert.ok(docs.indexOf('## Everyday Commands') < docs.indexOf('## Configuration and Recovery'));
  assert.ok(docs.indexOf('## Configuration and Recovery') < docs.indexOf('## Security, Networking, and Manual Automation'));
  assert.match(docs, /sync diagnose/);
  assert.match(docs, /resync preview/);

  console.log('Headless installer, helper, and documentation verified');
} finally {
  await rm(testRoot, { recursive: true, force: true });
}
