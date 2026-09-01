import { cleanup, fireEvent, render, screen } from '@testing-library/preact';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { getCurrent, getVersion, handlers, invoke, listen, onOpenUrl } = vi.hoisted(() => ({
  getCurrent: vi.fn(),
  getVersion: vi.fn(),
  handlers: {},
  invoke: vi.fn(),
  listen: vi.fn(),
  onOpenUrl: vi.fn(),
}));

vi.mock('@tauri-apps/api/core', () => ({ invoke }));
vi.mock('@tauri-apps/api/app', () => ({ getVersion }));
vi.mock('@tauri-apps/api/event', () => ({ listen }));
vi.mock('@tauri-apps/plugin-deep-link', () => ({ getCurrent, onOpenUrl }));

import { App, Dashboard, Setup, activationDecisions, formatDate, resyncDecisions, statusTone } from './main.jsx';

const status = {
  connected: true, machineHost: 'gaggimate.local', machineReachable: true, syncing: false,
  lastSyncAt: null, lastError: null, profiles: 2, shots: 10, notes: 3, conflicts: 1,
  suppressed: 2, initialSyncConfigured: true, duplicatePolicy: 'reuse_matching',
  notesSyncStatus: 'one_way', notesSyncTargetDeviceId: null, notesSyncWriterDeviceId: null,
  thisDeviceId: 'this-device', notesSyncIntroSeen: true, noteBackups: [], issues: [],
};

beforeEach(() => {
  Object.keys(handlers).forEach(key => delete handlers[key]);
  getCurrent.mockReset();
  getCurrent.mockResolvedValue([]);
  getVersion.mockReset();
  getVersion.mockResolvedValue('0.3.12');
  onOpenUrl.mockReset();
  onOpenUrl.mockImplementation(callback => {
    handlers.deepLink = callback;
    return Promise.resolve(() => {});
  });
  listen.mockReset();
  listen.mockImplementation((event, callback) => {
    handlers[event] = callback;
    return Promise.resolve(() => {});
  });
  invoke.mockReset();
  invoke.mockImplementation(command => {
    if (command === 'get_autostart_status') return Promise.resolve({ enabled: true, requiresWindowsSettings: false, blockedByPolicy: false, migrationAvailable: false });
    if (command === 'get_hide_app_icon') return Promise.resolve(false);
    if (command === 'get_update_status') return Promise.resolve({ kind: 'unknown' });
    return Promise.resolve(undefined);
  });
});

afterEach(cleanup);

describe('dashboard decisions', () => {
  it('keeps MyBrewFolio preselected only for differing activation Notes', () => {
    expect(activationDecisions({ items: [
      { sourceKey: 'one', differs: true },
      { sourceKey: 'two', differs: false },
    ] })).toEqual({ one: 'mybrewfolio' });
  });

  it('creates the safe default resync decisions', () => {
    expect(resyncDecisions({
      restoreItems: [{ id: 'restore-one' }],
      duplicates: [{ mapping_id: 'mapping', keep_shot_id: 'old', remove_shot_id: 'new', notes_conflict: true }],
    })).toEqual({
      restoreIds: ['restore-one'],
      duplicateDecisions: [{ mappingId: 'mapping', keepShotId: 'old', removeShotId: 'new', selected: true, notesResolution: '' }],
    });
  });

  it('uses deterministic status priority', () => {
    expect(statusTone('sync', 'Saved', 'success', 'Error')).toBe('working');
    expect(statusTone('', 'Saved', 'success', 'Error')).toBe('success');
    expect(statusTone('', '', 'success', 'Error')).toBe('error');
    expect(statusTone('', '', 'success', '')).toBe('info');
  });

  it('formats missing and invalid dates safely', () => {
    expect(formatDate(null)).toBe('Not synced yet');
    expect(formatDate('not-a-date')).toBe('Not synced yet');
  });
});

describe('Sync interface', () => {
  it('connects with the selected machine address', async () => {
    const refresh = vi.fn();
    render(<Setup status={status} refresh={refresh} externalNotice={null} />);
    fireEvent.input(screen.getByPlaceholderText('gaggimate.local'), { target: { value: '192.168.1.42' } });
    fireEvent.click(screen.getByRole('button', { name: 'Connect MyBrewFolio' }));
    await vi.waitFor(() => expect(invoke).toHaveBeenNthCalledWith(1, 'set_machine_host', { host: '192.168.1.42' }));
    expect(invoke).toHaveBeenNthCalledWith(2, 'begin_oauth');
  });

  it('keeps the setup screen usable when connecting fails', async () => {
    invoke.mockImplementation(command => command === 'set_machine_host'
      ? Promise.reject(new Error('GaggiMate is unreachable'))
      : Promise.resolve(undefined));
    render(<Setup status={status} refresh={vi.fn()} externalNotice={{ message: 'Previous connection was removed.' }} />);
    fireEvent.click(screen.getByRole('button', { name: 'Connect MyBrewFolio' }));
    await vi.waitFor(() => expect(screen.getByText('Error: GaggiMate is unreachable')).toBeTruthy());
  });

  it('renders normal controls and asks before disconnecting', () => {
    render(<Dashboard status={status} refresh={vi.fn()} onDisconnected={vi.fn()} disconnectRequestToken={0} />);
    expect(screen.getByText('10')).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Set up two-way Notes Sync' })).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Disconnect account' }));
    expect(screen.getByText('Disconnect this computer?')).toBeTruthy();
  });

  it('opens Notes activation with MyBrewFolio preselected', async () => {
    const activation = { ...status, notesSyncStatus: 'activation_pending', notesSyncTargetDeviceId: 'this-device' };
    invoke.mockImplementation(command => {
      if (command === 'get_autostart_status') return Promise.resolve({ enabled: true, requiresWindowsSettings: false, blockedByPolicy: false, migrationAvailable: false });
      if (command === 'begin_two_way_notes_activation') return Promise.resolve({ backupId: 'backup', items: [{ sourceKey: 'shot:1', displayName: 'Shot 1', differs: true }] });
      return Promise.resolve(undefined);
    });
    render(<Dashboard status={activation} refresh={vi.fn()} onDisconnected={vi.fn()} disconnectRequestToken={0} />);
    await vi.waitFor(() => expect(screen.getByText('Review Notes to sync')).toBeTruthy());
    expect(screen.getByRole('combobox', { name: 'Notes source for Shot 1' }).value).toBe('mybrewfolio');
  });

  it('restores only available Notes from a selected backup', async () => {
    const backup = { id: 'backup-1', slot: 'latest', itemCount: 2, createdAt: '2026-08-20T10:00:00Z' };
    invoke.mockImplementation(command => {
      if (command === 'get_autostart_status') return Promise.resolve({ enabled: true, requiresWindowsSettings: false, blockedByPolicy: false, migrationAvailable: false });
      if (command === 'get_hide_app_icon') return Promise.resolve(false);
      if (command === 'check_update') return Promise.resolve('up-to-date');
      if (command === 'preview_notes_restore') return Promise.resolve({ items: [
        { source_key: 'shot:1', available: true },
        { sourceKey: 'shot:2', available: false },
      ] });
      if (command === 'restore_notes_backup') return Promise.resolve({ applied: 1, skipped: 1 });
      return Promise.resolve(undefined);
    });
    render(<Dashboard status={{ ...status, noteBackups: [backup] }} refresh={vi.fn()} onDisconnected={vi.fn()} disconnectRequestToken={0} />);
    fireEvent.click(screen.getByRole('button', { name: 'Restore' }));
    await vi.waitFor(() => expect(screen.getByText('Restore GaggiMate Notes')).toBeTruthy());
    expect(screen.getByRole('checkbox', { name: 'shot:2 · no longer available on this GaggiMate' }).disabled).toBe(true);
    fireEvent.click(screen.getByRole('button', { name: 'Restore selected Notes' }));
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('restore_notes_backup', {
      backupId: 'backup-1', sourceKeys: ['shot:1'],
    }));
    await vi.waitFor(() => expect(screen.getByText('Notes restore finished. 1 restored, 1 skipped.')).toBeTruthy());
  });

  it('requires a Notes decision and a second confirmation before applying a resync', async () => {
    invoke.mockImplementation(command => {
      if (command === 'get_autostart_status') return Promise.resolve({ enabled: true, requiresWindowsSettings: false, blockedByPolicy: false, migrationAvailable: false });
      if (command === 'get_hide_app_icon') return Promise.resolve(false);
      if (command === 'check_update') return Promise.resolve('up-to-date');
      if (command === 'preview_complete_resync') return Promise.resolve({
        restoreItems: [{ id: 'restore-1', kind: 'shot', sourceKey: 'shot:1' }],
        duplicates: [{ mapping_id: 'mapping-1', keep_shot_id: 'existing', remove_shot_id: 'copy', keep_name: 'Existing', remove_name: 'Copy', mapped_name: 'Shot 1', notes_conflict: true }],
        ambiguousDuplicates: [{ mapping_id: 'ambiguous', mapped_name: 'Shot 2', candidate_count: 2 }],
      });
      if (command === 'apply_complete_resync') return Promise.resolve({ restored: 1, merged: 1 });
      return Promise.resolve(undefined);
    });
    render(<Dashboard status={status} refresh={vi.fn()} onDisconnected={vi.fn()} disconnectRequestToken={0} />);
    fireEvent.click(screen.getByRole('button', { name: 'Complete resync' }));
    await vi.waitFor(() => expect(screen.getByText('Complete resync preview')).toBeTruthy());
    fireEvent.click(screen.getByRole('button', { name: 'Apply complete resync' }));
    expect(screen.getByText('Choose which Notes to keep for every selected Notes conflict.')).toBeTruthy();
    fireEvent.change(screen.getByRole('combobox', { name: 'Notes resolution for Shot 1' }), { target: { value: 'gaggimate' } });
    fireEvent.click(screen.getByRole('button', { name: 'Apply complete resync' }));
    expect(screen.getByText('Confirm restoring 1 selected machine items and merging 1 selected duplicate shots.')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Confirm complete resync' }));
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('apply_complete_resync', {
      decisions: {
        restoreItemIds: ['restore-1'],
        duplicateResolutions: [{ mappingId: 'mapping-1', keepShotId: 'existing', removeShotId: 'copy', notesResolution: 'gaggimate' }],
      },
    }));
    await vi.waitFor(() => expect(screen.getByText('Complete resync finished. 1 restored, 1 duplicates merged.')).toBeTruthy());
  });

  it('saves first-sync policy and exposes update and startup feedback', async () => {
    invoke.mockImplementation(command => {
      if (command === 'get_autostart_status') return Promise.resolve({ enabled: false, requiresWindowsSettings: true, blockedByPolicy: false, migrationAvailable: true });
      if (command === 'get_hide_app_icon') return Promise.resolve(false);
      if (command === 'get_update_status') return Promise.resolve({ kind: 'available', version: '0.3.13', promptPending: true });
      if (command === 'install_update') return Promise.resolve({ kind: 'upToDate' });
      return Promise.resolve(undefined);
    });
    render(<Dashboard status={{ ...status, initialSyncConfigured: false, duplicatePolicy: 'import_all' }} refresh={vi.fn()} onDisconnected={vi.fn()} disconnectRequestToken={0} />);
    await vi.waitFor(() => expect(screen.getByText('Update available')).toBeTruthy());
    expect(screen.getByText('Update 0.3.13 is available.')).toBeTruthy();
    expect(screen.getByText(/Windows needs a one-time confirmation/)).toBeTruthy();
    expect(screen.getByText(/Windows has disabled startup/)).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Save and start first sync' }));
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('configure_sync', { reuseMatching: false }));
    fireEvent.click(screen.getByRole('button', { name: 'Install update' }));
    await vi.waitFor(() => expect(screen.getByText('MyBrewFolio Sync is up to date.')).toBeTruthy());
  });

  it('lets the user defer an available update until the next daily reminder', async () => {
    invoke.mockImplementation(command => {
      if (command === 'get_autostart_status') return Promise.resolve({ enabled: true, requiresWindowsSettings: false, blockedByPolicy: false, migrationAvailable: false });
      if (command === 'get_hide_app_icon') return Promise.resolve(false);
      if (command === 'get_update_status') return Promise.resolve({ kind: 'available', version: '0.4.3', promptPending: true });
      if (command === 'dismiss_update') return Promise.resolve({ kind: 'available', version: '0.4.3', promptPending: false });
      return Promise.resolve(undefined);
    });
    render(<Dashboard status={status} refresh={vi.fn()} onDisconnected={vi.fn()} disconnectRequestToken={0} />);
    await vi.waitFor(() => expect(screen.getByRole('alertdialog', { name: 'Update available' })).toBeTruthy());
    fireEvent.click(screen.getByRole('button', { name: 'Later' }));
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('dismiss_update'));
    await vi.waitFor(() => expect(screen.queryByRole('alertdialog', { name: 'Update available' })).toBeNull());
    expect(screen.getByText('Update 0.4.3 is available.')).toBeTruthy();
  });

  it('offers a restart after installation and explains a deferred restart', async () => {
    invoke.mockImplementation(command => {
      if (command === 'get_autostart_status') return Promise.resolve({ enabled: true, requiresWindowsSettings: false, blockedByPolicy: false, migrationAvailable: false });
      if (command === 'get_hide_app_icon') return Promise.resolve(false);
      if (command === 'get_update_status') return Promise.resolve({ kind: 'available', version: '0.4.3', promptPending: true });
      if (command === 'install_update') return Promise.resolve({ kind: 'installed', version: '0.4.3', restartRequested: false, restartWaitingForSync: false });
      if (command === 'restart_after_update') return Promise.resolve({ kind: 'installed', version: '0.4.3', restartRequested: true, restartWaitingForSync: true });
      return Promise.resolve(undefined);
    });
    render(<Dashboard status={{ ...status, syncing: true }} refresh={vi.fn()} onDisconnected={vi.fn()} disconnectRequestToken={0} />);
    await vi.waitFor(() => expect(screen.getByRole('button', { name: 'Install update' })).toBeTruthy());
    fireEvent.click(screen.getByRole('button', { name: 'Install update' }));
    await vi.waitFor(() => expect(screen.getByRole('alertdialog', { name: 'Update installed' })).toBeTruthy());
    fireEvent.click(screen.getByRole('button', { name: 'Restart Sync' }));
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('restart_after_update'));
    await vi.waitFor(() => expect(screen.getByText('Restarting after the current synchronization finishes.')).toBeTruthy());
  });

  it('keeps Microsoft Store updates outside the custom updater flow', async () => {
    invoke.mockImplementation(command => {
      if (command === 'get_autostart_status') return Promise.resolve({ enabled: true, requiresWindowsSettings: false, blockedByPolicy: false, migrationAvailable: false });
      if (command === 'get_hide_app_icon') return Promise.resolve(false);
      if (command === 'get_update_status') return Promise.resolve({ kind: 'storeManaged' });
      return Promise.resolve(undefined);
    });
    render(<Dashboard status={status} refresh={vi.fn()} onDisconnected={vi.fn()} disconnectRequestToken={0} />);
    await vi.waitFor(() => expect(screen.getByText('Updates are managed by Microsoft Store.')).toBeTruthy());
    expect(screen.queryByRole('alertdialog', { name: 'Update available' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Check for updates' })).toBeNull();
  });

  it('shows an English status message rather than an updater error', async () => {
    invoke.mockImplementation(command => {
      if (command === 'get_autostart_status') return Promise.resolve({ enabled: true, requiresWindowsSettings: false, blockedByPolicy: false, migrationAvailable: false });
      if (command === 'get_hide_app_icon') return Promise.resolve(false);
      if (command === 'get_update_status') return Promise.resolve({ kind: 'unknown' });
      if (command === 'check_update') return Promise.reject(new Error('updater metadata failed'));
      return Promise.resolve(undefined);
    });
    render(<Dashboard status={status} refresh={vi.fn()} onDisconnected={vi.fn()} disconnectRequestToken={0} />);
    await vi.waitFor(() => expect(screen.getByRole('button', { name: 'Check for updates' })).toBeTruthy());
    fireEvent.click(screen.getByRole('button', { name: 'Check for updates' }));
    await vi.waitFor(() => expect(screen.getByText('Unable to check for updates. Sync will try again later.')).toBeTruthy());
    expect(screen.queryByText(/updater metadata failed/)).toBeNull();
  });

  it('shows successful sync, machine address, app icon and help interactions', async () => {
    render(<Dashboard status={status} refresh={vi.fn()} onDisconnected={vi.fn()} disconnectRequestToken={0} />);
    await vi.waitFor(() => expect(screen.getByText('Installed version 0.3.12')).toBeTruthy());
    fireEvent.click(screen.getByRole('button', { name: 'Sync now' }));
    await vi.waitFor(() => expect(screen.getByText('Synchronization completed.')).toBeTruthy());
    const hostField = screen.getByRole('textbox', { name: 'GaggiMate hostname or local IP' });
    fireEvent.input(hostField, { target: { value: '192.168.1.44' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('set_machine_host', { host: '192.168.1.44' }));
    await vi.waitFor(() => expect(screen.getByText('Machine address saved.')).toBeTruthy());
    fireEvent.click(screen.getByRole('checkbox', { name: 'Hide app icon from Dock or taskbar' }));
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('set_hide_app_icon', { hidden: true }));
    await vi.waitFor(() => expect(screen.getByText('App icon hidden. Use the menu bar or tray icon to open Sync.')).toBeTruthy());
    fireEvent.click(screen.getByRole('button', { name: 'Explain complete resync' }));
    expect(screen.getByText(/Reads the complete GaggiMate library again/)).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Support' }));
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('open_mybrewfolio_page', { page: 'syncHelp' }));
  });

  it('refreshes the app through status and tray events', async () => {
    invoke.mockImplementation(command => {
      if (command === 'get_status') return Promise.resolve(status);
      if (command === 'get_autostart_status') return Promise.resolve({ enabled: true, requiresWindowsSettings: false, blockedByPolicy: false, migrationAvailable: false });
      if (command === 'get_hide_app_icon') return Promise.resolve(false);
      if (command === 'check_update') return Promise.resolve('up-to-date');
      return Promise.resolve(undefined);
    });
    render(<App />);
    await vi.waitFor(() => expect(screen.getByText('Connected')).toBeTruthy());
    await vi.waitFor(() => expect(handlers['sync-requested']).toBeTypeOf('function'));
    handlers['sync-requested']();
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('sync_now'));
    handlers['disconnect-confirmation-requested']();
    await vi.waitFor(() => expect(screen.getByText('Disconnect this computer?')).toBeTruthy());
  });

  it('surfaces a deep-link sign-in error on the setup screen', async () => {
    getCurrent.mockResolvedValue(['mybrewfolio-sync://oauth/callback?code=example']);
    invoke.mockImplementation(command => {
      if (command === 'get_status') return Promise.resolve({ ...status, connected: false });
      if (command === 'complete_oauth') return Promise.reject(new Error('Authorization was cancelled'));
      return Promise.resolve(undefined);
    });
    render(<App />);
    await vi.waitFor(() => expect(screen.getByText('MyBrewFolio could not finish connecting this installation: Error: Authorization was cancelled')).toBeTruthy());
  });

  it('explains when a local disconnect cannot remove credentials', async () => {
    let refreshCount = 0;
    invoke.mockImplementation(command => {
      if (command === 'get_status') {
        refreshCount += 1;
        return Promise.resolve(refreshCount === 1 ? status : { ...status, connected: false });
      }
      if (command === 'get_autostart_status') return Promise.resolve({ enabled: true, requiresWindowsSettings: false, blockedByPolicy: false, migrationAvailable: false });
      if (command === 'get_hide_app_icon') return Promise.resolve(false);
      if (command === 'check_update') return Promise.resolve('up-to-date');
      if (command === 'disconnect_account') return Promise.resolve({ credentialsRemoved: false, serverRevoked: false });
      return Promise.resolve(undefined);
    });
    render(<App />);
    await vi.waitFor(() => expect(screen.getByRole('button', { name: 'Disconnect account' })).toBeTruthy());
    fireEvent.click(screen.getByRole('button', { name: 'Disconnect account' }));
    fireEvent.click(screen.getByRole('button', { name: 'Disconnect' }));
    await vi.waitFor(() => expect(screen.getByText('Disconnected, but the stored sign-in could not be removed.')).toBeTruthy());
    fireEvent.click(screen.getByRole('button', { name: 'Get help' }));
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('open_mybrewfolio_page', { page: 'syncHelp' }));
  });
});
