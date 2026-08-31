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
  const shot = await fetch(`http://127.0.0.1:${port}/api/history/000001.slog`);
  const shotBytes = await shot.arrayBuffer();
  const shotView = new DataView(shotBytes);
  if (
    !shot.ok ||
    shotView.getUint8(4) !== 7 ||
    shotView.getUint8(5) !== 30 ||
    shotView.getUint32(542, true) !== 263 ||
    shotView.getUint16(570, true) !== 151
  ) {
    throw new Error('Version-seven shot fixture is invalid');
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
        resolve(value.error ? null : value.notes);
      }
    });
    socket.addEventListener('error', reject);
  });
  const profileRequest = payload => new Promise((resolve, reject) => {
    const socket = new WebSocket(`ws://127.0.0.1:${port}/ws`);
    const rid = `profile-${Math.random().toString(16).slice(2)}`;
    socket.addEventListener('open', () => socket.send(JSON.stringify({ ...payload, rid })));
    socket.addEventListener('message', event => {
      const value = JSON.parse(String(event.data));
      if (value.rid === rid) { socket.close(); resolve(value); }
    });
    socket.addEventListener('error', reject);
  });
  const notes = await readNotes('1');
  if (notes.rating !== 4) throw new Error('Notes fixture is invalid');
  const nullNotes = await readNotes('2');
  if (nullNotes !== null) throw new Error('Null notes fixture is invalid');
  const emptyNotes = await readNotes('999');
  if (emptyNotes !== null) throw new Error('Missing notes fixture is invalid');
  await new Promise((resolve, reject) => {
    const socket = new WebSocket(`ws://127.0.0.1:${port}/ws`);
    socket.addEventListener('open', () => socket.send(JSON.stringify({
      tp: 'req:history:notes:save', rid: 'save-notes', id: '1', notes: { rating: 5, notes: 'Written by Sync' }
    })));
    socket.addEventListener('message', event => {
      const value = JSON.parse(String(event.data));
      if (value.rid === 'save-notes') { socket.close(); resolve(); }
    });
    socket.addEventListener('error', reject);
  });
  const writtenNotes = await readNotes('1');
  if (writtenNotes.notes !== 'Written by Sync') throw new Error('Notes save fixture is invalid');

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
  const installed = { ...profile, id: 'store-profile', label: 'Store profile' };
  await profileRequest({ tp: 'req:profiles:save', profile: installed });
  await profileRequest({ tp: 'req:profiles:favorite', id: installed.id });
  await profileRequest({ tp: 'req:profiles:select', id: installed.id });
  const installedReload = await profileRequest({ tp: 'req:profiles:load', id: installed.id });
  if (!installedReload.profile?.favorite || !installedReload.profile?.selected) {
    throw new Error('Profile install actions were not persisted by the fixture');
  }
  const inventory = await profileRequest({ tp: 'req:profiles:list', minimal: true });
  if (!inventory.profiles.some(item => item.id === installed.id && item.favorite && item.selected)) {
    throw new Error('Profile inventory fixture is invalid');
  }
  console.log('Fake GaggiMate fixtures verified');
} finally {
  child.kill();
}
