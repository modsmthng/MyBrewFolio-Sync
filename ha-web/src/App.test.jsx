// SPDX-License-Identifier: GPL-3.0-or-later
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/preact';
import { App } from './App.jsx';
import { pairingLink, request } from './api.js';
const status = { connected: true, machines: [] };
const reply = body => Promise.resolve({ ok: true, json: async () => body });
beforeEach(() => vi.stubGlobal('fetch', vi.fn((url) => reply(url === './api/machines' ? { machines: [{ id: 'existing-id', name: 'Kitchen' }] } : { status, setup: { state: 'machine_selection' }, csrfToken: 'csrf', version: '0.6.6' }))));
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });
describe('Home Assistant setup', () => {
  it('opens the cloud pairing page in another tab without exposing credentials', async () => {
    fetch.mockImplementation(() => reply({ status: { connected: false, machines: [] }, setup: { state: 'pairing', verificationUrl: 'https://mybrewfolio.com/v1/sync/device-auth/authorize?user_code=abc' }, csrfToken: 'csrf' }));
    render(<App />);
    const link = await screen.findByRole('link', { name: 'Connect MyBrewFolio ↗' });
    expect(link.target).toBe('_blank');
    expect(link.href).toContain('user_code=abc');
    expect(screen.queryByText('Connect a machine')).toBeNull();
  });
  it('connects an existing machine with a deliberate choice and a local address', async () => {
    render(<App />);
    await screen.findByRole('option', { name: 'Kitchen' });
    fireEvent.change(screen.getByLabelText('Account machine'), { target: { value: 'existing-id' } });
    fireEvent.input(screen.getByLabelText('Local address'), { target: { value: '192.168.1.42' } });
    fireEvent.click(screen.getByRole('button', { name: 'Connect machine' }));
    await waitFor(() => expect(fetch).toHaveBeenCalledWith('./api/action', expect.objectContaining({
      headers: expect.objectContaining({ 'X-MyBrewFolio-CSRF': 'csrf' }),
      body: JSON.stringify({ action: 'connectMachine', machineId: 'existing-id', host: '192.168.1.42' }),
    })));
  });
  it('allows a new machine for an empty account', async () => {
    render(<App />);
    await screen.findByLabelText('Machine selection');
    fireEvent.change(screen.getByLabelText('Machine selection'), { target: { value: 'new' } });
    fireEvent.input(screen.getByLabelText('New machine name'), { target: { value: 'Office' } });
    fireEvent.input(screen.getByLabelText('Local address'), { target: { value: 'gaggimate.local' } });
    fireEvent.click(screen.getByRole('button', { name: 'Connect machine' }));
    await waitFor(() => expect(fetch).toHaveBeenCalledWith('./api/action', expect.objectContaining({ body: JSON.stringify({ action: 'addMachine', name: 'Office', host: 'gaggimate.local' }) })));
  });
  it('requires confirmation before disconnecting the account', async () => {
    render(<App />);
    fireEvent.click(await screen.findByRole('button', { name: 'Disconnect account' }));
    expect(fetch.mock.calls.some(([, options]) => options.body)).toBe(false);
    fireEvent.click(screen.getByRole('button', { name: 'Confirm disconnect' }));
    await waitFor(() => expect(fetch).toHaveBeenCalledWith('./api/action', expect.objectContaining({ body: JSON.stringify({ action: 'disconnect', confirm: true }) })));
  });
  it('displays registration failures instead of claiming the installation is ready', async () => {
    fetch.mockImplementation(() => reply({ status, setup: { state: 'error', error: 'Registration unavailable' }, csrfToken: 'csrf' }));
    render(<App />);
    await screen.findByText(/Registration unavailable/);
    expect(screen.queryByText('Connect a machine')).toBeNull();
  });
  it('displays actionable HTTP failures', async () => {
    fetch.mockResolvedValue({ ok: false, status: 403, json: async () => ({}) });
    await expect(request('status')).rejects.toThrow(/Reload this page/);
  });
  it('shows multiple machines and keeps edits and confirmed removal scoped to the chosen one', async () => {
    let machines = [
      { machineId: 'kitchen-id', name: 'Kitchen', machineHost: '192.168.1.41', machineReachable: false, initialSyncConfigured: false, lastError: 'Machine is offline', shots: 5, profiles: 2, notes: 3 },
      { machineId: 'office-id', name: 'Office', machineHost: '192.168.1.42', machineReachable: true, initialSyncConfigured: true, shots: 10, profiles: 3, notes: 4 },
    ];
    fetch.mockImplementation((url, options) => {
      if (url === './api/action') {
        const body = JSON.parse(options.body);
        machines = body.action === 'removeMachine' ? machines.filter(machine => machine.machineId !== body.machineId)
          : machines.map(machine => machine.machineId === body.machineId ? { ...machine, ...(body.name ? { name: body.name } : {}), ...(body.host ? { machineHost: body.host } : {}) } : machine);
        return reply({ ok: true });
      }
      return reply(url === './api/machines' ? { machines: [] } : { status: { connected: true, machines }, setup: { state: 'ready' }, csrfToken: 'csrf' });
    });
    render(<App />);
    const office = within((await screen.findByRole('heading', { name: 'Office' })).closest('section'));
    expect(screen.getByText('Machine is offline')).toBeTruthy();
    expect(screen.getByText(/Choose your import preference/)).toBeTruthy();
    fireEvent.input(office.getByLabelText('Machine name'), { target: { value: 'Work' } });
    fireEvent.click(office.getByRole('button', { name: 'Save name' }));
    await screen.findByRole('heading', { name: 'Work' });
    fireEvent.input(office.getByLabelText('Local address'), { target: { value: '192.168.1.43' } });
    await waitFor(() => expect(office.getByRole('button', { name: 'Save address' }).disabled).toBe(false));
    fireEvent.click(office.getByRole('button', { name: 'Save address' }));
    await waitFor(() => expect(machines[1].machineHost).toBe('192.168.1.43'));
    await waitFor(() => expect(office.getByRole('button', { name: 'Remove connection' }).disabled).toBe(false));
    fireEvent.click(office.getByRole('button', { name: 'Remove connection' }));
    fireEvent.click(office.getByRole('button', { name: 'Cancel' }));
    expect(machines).toHaveLength(2);
    fireEvent.click(office.getByRole('button', { name: 'Remove connection' }));
    fireEvent.click(office.getByRole('button', { name: 'Confirm removal' }));
    await waitFor(() => expect(screen.queryByRole('heading', { name: 'Work' })).toBeNull());
    expect(machines[0].name).toBe('Kitchen');
    expect(machines[0].machineHost).toBe('192.168.1.41');
    expect(fetch).toHaveBeenCalledWith('./api/action', expect.objectContaining({ body: JSON.stringify({ action: 'removeMachine', machineId: 'office-id', confirm: true }) }));
  });

  it('allows only the exact production cloud pairing endpoint', () => {
    expect(pairingLink('https://mybrewfolio.com/v1/sync/device-auth/authorize?user_code=abc')).toContain('user_code=abc');
    for (const url of ['javascript:alert(1)', 'https://evil.test/v1/sync/device-auth/authorize', 'https://mybrewfolio.com.evil.test/v1/sync/device-auth/authorize', 'https://mybrewfolio.com/other', 'http://mybrewfolio.com/v1/sync/device-auth/authorize']) expect(pairingLink(url)).toBeNull();
  });
});
