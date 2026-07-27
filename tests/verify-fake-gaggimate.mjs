// SPDX-License-Identifier: GPL-3.0-or-later

import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { WebSocket } from 'ws';

const port = 18088;
const child = spawn(process.execPath, ['tests/fake-gaggimate.mjs'], {
  cwd: new URL('..', import.meta.url),
  env: { ...process.env, FAKE_GAGGIMATE_PORT: String(port) },
  stdio: ['ignore', 'pipe', 'inherit']
});

try {
  await Promise.race([
    once(child.stdout, 'data'),
    new Promise((_, reject) => setTimeout(() => reject(new Error('Fake GaggiMate did not start')), 5000))
  ]);
  const index = await fetch(`http://127.0.0.1:${port}/api/history/index.bin`);
  if (!index.ok || (await index.arrayBuffer()).byteLength !== 160) {
    throw new Error('Shot index fixture is invalid');
  }
  const readNotes = id => new Promise((resolve, reject) => {
    const socket = new WebSocket(`ws://127.0.0.1:${port}/ws`);
    socket.addEventListener('open', () => socket.send(JSON.stringify({
      tp: 'req:history:notes:get', rid: `notes-${id}`, id
    })));
    socket.addEventListener('message', event => {
      const value = JSON.parse(String(event.data));
      if (value.rid === `notes-${id}`) {
        socket.close();
        resolve(value.notes);
      }
    });
    socket.addEventListener('error', reject);
  });
  const notes = await readNotes('1');
  if (notes.rating !== 4) throw new Error('Notes fixture is invalid');
  const emptyNotes = await readNotes('999');
  if (Object.keys(emptyNotes).length !== 0) throw new Error('Empty notes fixture is invalid');

  const profile = await new Promise((resolve, reject) => {
    const socket = new WebSocket(`ws://127.0.0.1:${port}/ws`);
    const timer = setTimeout(() => reject(new Error('Profile fixture timed out')), 5000);
    socket.on('open', () => socket.send(JSON.stringify({
      tp: 'req:profiles:load',
      rid: 'fixture-request',
      id: 'sync-fixture-profile'
    })));
    socket.on('message', data => {
      clearTimeout(timer);
      const response = JSON.parse(data.toString());
      socket.close();
      resolve(response.profile);
    });
    socket.on('error', reject);
  });
  if (profile?.id !== 'sync-fixture-profile') throw new Error('Profile fixture is invalid');
  console.log('Fake GaggiMate fixtures verified');
} finally {
  child.kill();
}
