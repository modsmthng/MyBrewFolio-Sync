// SPDX-License-Identifier: GPL-3.0-or-later
import { defineConfig } from 'vite';
import preact from '@preact/preset-vite';
export default defineConfig({
  root: 'ha-web',
  base: './',
  plugins: [preact()],
  build: { outDir: '../dist-ha', emptyOutDir: true, target: 'safari15' },
});
