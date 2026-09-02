// SPDX-License-Identifier: GPL-3.0-or-later

import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { chmod, mkdtemp, mkdir, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { platform, tmpdir } from 'node:os';
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
  const installDir = join(testRoot, 'installation with spaces');
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

  const beforeNoTty = await readFile(dockerLog, 'utf8');
  const noTty = await run(join(installDir, 'sync'), ['notes', 'enable'], { env: environment });
  assert.equal(noTty.code, 64);
  assert.match(noTty.stderr, /interactive terminal/);
  assert.equal(await readFile(dockerLog, 'utf8'), beforeNoTty, 'no Docker/API calls without a terminal');

  const quote = value => `'${value.replaceAll("'", "'\\''")}'`;
  const terminalArgs = platform() === 'darwin'
    ? ['-q', '/dev/null', join(installDir, 'sync'), 'notes', 'enable']
    : ['-q', '-e', '-c', `${quote(join(installDir, 'sync'))} notes enable`, '/dev/null'];
  const terminal = await run('script', terminalArgs, { env: environment });
  assert.equal(terminal.code, 0, terminal.stderr);
  const terminalCalls = await readFile(dockerLog, 'utf8');
  assert.match(terminalCalls, /exec -e MYBREWFOLIO_SYNC_CLI_INSTALL_DIR=.* sync mybrewfolio-syncd notes enable/);
  assert.doesNotMatch(terminalCalls, /exec -T .*notes enable/);

  const status = await run(join(installDir, 'sync'), ['status'], { env: environment });
  assert.equal(status.code, 0, status.stderr);
  assert.match(await readFile(dockerLog, 'utf8'), /exec -T sync mybrewfolio-syncd status/);

  const retainedNames = ['compose.yaml', '.env', 'state.key'];
  const retained = await Promise.all(retainedNames.map(name => readFile(join(installDir, name))));
  const callsBeforeUpdate = await readFile(dockerLog, 'utf8');
  await writeFile(join(installDir, 'sync'), '#!/bin/sh\n# Old helper\n');
  for (let attempt = 0; attempt < 2; attempt += 1) {
    const update = await run('sh', [installerPath, '--update-helper'], { env: environment });
    assert.equal(update.code, 0, update.stderr);
    assert.match(update.stdout, /Updated helper:/);
    assert.match(await readFile(join(installDir, 'sync'), 'utf8'), /notes:enable/);
    assert.equal((await stat(join(installDir, 'sync'))).mode & 0o777, 0o700);
    for (const [index, name] of retainedNames.entries()) {
      assert.deepEqual(await readFile(join(installDir, name)), retained[index]);
    }
    assert.equal(await readFile(dockerLog, 'utf8'), callsBeforeUpdate, 'helper update does not invoke Docker');
  }
  const mixedUpdate = await run('sh', [installerPath, '--update-helper', '--host', 'changed'], { env: environment });
  assert.equal(mixedUpdate.code, 64);
  const missingUpdate = await run('sh', [installerPath, '--update-helper'], {
    env: { ...environment, MYBREWFOLIO_SYNC_HOME: join(testRoot, 'not-installed') }
  });
  assert.equal(missingUpdate.code, 73);

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
  assert.ok(docs.indexOf('## Quick Start') < docs.indexOf('## Two-way Notes Sync'));
  assert.ok(docs.indexOf('## Two-way Notes Sync') < docs.indexOf('## Everyday Commands'));
  assert.match(docs, /sync notes enable/);
  assert.match(docs, /--update-helper/);
  assert.ok(docs.indexOf('## Quick Start') < docs.indexOf('## Everyday Commands'));
  assert.ok(docs.indexOf('## Everyday Commands') < docs.indexOf('## Configuration and Recovery'));
  assert.ok(docs.indexOf('## Configuration and Recovery') < docs.indexOf('## Security, Networking, and Manual Automation'));
  assert.match(docs, /sync diagnose/);
  assert.match(docs, /resync preview/);

  console.log('Headless installer, helper, and documentation verified');
} finally {
  await rm(testRoot, { recursive: true, force: true });
}
