// SPDX-License-Identifier: GPL-3.0-or-later

import http from 'node:http';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { WebSocketServer } from 'ws';

const fixture = JSON.parse(await readFile(new URL('./fixtures/library.json', import.meta.url), 'utf8'));
const port = Number.parseInt(process.env.FAKE_GAGGIMATE_PORT || '8088', 10);

function fixedString(buffer, offset, length, value) {
  buffer.write(String(value).slice(0, length - 1), offset, length - 1, 'utf8');
}

function shotIndex() {
  const buffer = Buffer.alloc(32 + 128);
  buffer.writeUInt32LE(0x58444953, 0);
  buffer.writeUInt16LE(1, 4);
  buffer.writeUInt16LE(128, 6);
  buffer.writeUInt32LE(1, 8);
  buffer.writeUInt32LE(2, 12);
  const entry = 32;
  buffer.writeUInt32LE(1, entry);
  buffer.writeUInt32LE(1_735_689_600, entry + 4);
  buffer.writeUInt32LE(500, entry + 8);
  buffer.writeUInt16LE(360, entry + 12);
  buffer.writeUInt8(4, entry + 14);
  buffer.writeUInt8(0x05, entry + 15);
  fixedString(buffer, entry + 16, 32, fixture.profile.id);
  fixedString(buffer, entry + 48, 48, fixture.profile.label);
  return buffer;
}

function shotLog() {
  const sampleCount = 3;
  const buffer = Buffer.alloc(512 + sampleCount * 26);
  buffer.writeUInt32LE(0x544f4853, 0);
  buffer.writeUInt8(5, 4);
  buffer.writeUInt8(26, 5);
  buffer.writeUInt16LE(512, 6);
  buffer.writeUInt16LE(250, 8);
  buffer.writeUInt32LE(0x1fff, 12);
  buffer.writeUInt32LE(sampleCount, 16);
  buffer.writeUInt32LE(500, 20);
  buffer.writeUInt32LE(1_735_689_600, 24);
  fixedString(buffer, 28, 32, fixture.profile.id);
  fixedString(buffer, 60, 48, fixture.profile.label);
  buffer.writeUInt16LE(360, 108);
  buffer.writeUInt16LE(0, 110);
  buffer.writeUInt8(0, 112);
  fixedString(buffer, 114, 25, 'Bloom');
  buffer.writeUInt8(1, 458);
  buffer.writeUInt8(5, 459);
  buffer.writeUInt16LE(750, 460);

  const samples = [
    [0, 930, 925, 20, 18, 180, 200, 170, 0, 0, 0, 0, 0x000d],
    [1, 930, 928, 40, 39, 195, 200, 185, 0, 10, 10, 120, 0x000f],
    [2, 930, 931, 60, 58, 210, 200, 205, 0, 20, 20, 250, 0x000f]
  ];
  samples.forEach((values, sampleIndex) => {
    const base = 512 + sampleIndex * 26;
    values.forEach((value, fieldIndex) => {
      const signed = fieldIndex >= 5 && fieldIndex <= 8;
      if (signed) buffer.writeInt16LE(value, base + fieldIndex * 2);
      else buffer.writeUInt16LE(value, base + fieldIndex * 2);
    });
  });
  return buffer;
}

const indexFixture = shotIndex();
const shotFixture = shotLog();

const server = http.createServer((request, response) => {
  if (request.url === '/api/history/index.bin') {
    response.writeHead(200, { 'Content-Type': 'application/octet-stream' });
    response.end(indexFixture);
    return;
  }
  if (request.url === '/api/history/000001.slog') {
    response.writeHead(200, { 'Content-Type': 'application/octet-stream' });
    response.end(shotFixture);
    return;
  }
  if (request.url === '/api/history/000001.json') {
    response.writeHead(200, { 'Content-Type': 'application/json' });
    response.end(JSON.stringify(fixture.notes));
    return;
  }
  response.writeHead(404);
  response.end();
});

const websocket = new WebSocketServer({ server, path: '/ws' });
websocket.on('connection', socket => {
  socket.on('message', bytes => {
    let request;
    try {
      request = JSON.parse(bytes.toString());
    } catch {
      return;
    }
    if (request.tp === 'req:profiles:list') {
      socket.send(JSON.stringify({
        tp: 'res:profiles:list',
        rid: request.rid,
        profiles: [{ id: fixture.profile.id, label: fixture.profile.label }]
      }));
      return;
    }
    if (request.tp === 'req:profiles:load' && request.id === fixture.profile.id) {
      socket.send(JSON.stringify({
        tp: 'res:profiles:load',
        rid: request.rid,
        profile: fixture.profile
      }));
      return;
    }
    socket.send(JSON.stringify({ tp: 'res:error', rid: request.rid, error: 'Unsupported fixture request' }));
  });
});

server.listen(port, '127.0.0.1', () => {
  console.log(`Fake GaggiMate listening on 127.0.0.1:${port}`);
  console.log(`Use 127.0.0.1:${port} as the machine address in a development build.`);
});

process.on('SIGINT', () => server.close(() => process.exit(0)));
