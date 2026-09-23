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

import {
  App,
  Dashboard,
  FIRST_SYNCHRONIZATION_MESSAGE,
  Setup,
  firstSynchronizationInProgress,
  formatDate,
  statusTone,
  syncProgressMessage,
} from './main.jsx';

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
  it('explains a first synchronization while it is running', () => {
    expect(FIRST_SYNCHRONIZATION_MESSAGE).toBe(
      'Your first sync may take a while, depending on your history. You can leave this page and come back later. Keep the Sync app or Docker container running.',
    );
    expect(firstSynchronizationInProgress({ ...status, syncing: true })).toBe(true);
    expect(firstSynchronizationInProgress({ ...status, syncing: true, lastSyncAt: '2026-09-16T00:00:00Z' })).toBe(false);
    expect(firstSynchronizationInProgress({ ...status, syncing: true, lastError: 'The GaggiMate could not be reached' })).toBe(false);

    const view = render(<Dashboard status={{ ...status, syncing: true }} refresh={vi.fn()} onDisconnected={vi.fn()} disconnectRequestToken={0} />);
    expect(screen.getByText(FIRST_SYNCHRONIZATION_MESSAGE)).toBeTruthy();

    view.rerender(<Dashboard status={{ ...status, syncing: true, lastSyncAt: '2026-09-16T00:00:00Z' }} refresh={vi.fn()} onDisconnected={vi.fn()} disconnectRequestToken={0} />);
    expect(screen.queryByText(FIRST_SYNCHRONIZATION_MESSAGE)).toBeNull();
  });

  it('uses deterministic status priority', () => {
    expect(statusTone('sync', 'Saved', 'success', 'Error')).toBe('error');
    expect(statusTone('', 'Saved', 'success', 'Error')).toBe('error');
    expect(statusTone('', '', 'success', 'Error')).toBe('error');
    expect(statusTone('', '', 'success', '')).toBe('info');
  });

  it('shows safe first-sync progress for every phase', () => {
    expect(
      syncProgressMessage({ phase: 'reading_history', scannedShots: 8, totalShots: 20 }),
    ).toBe('Reading history: 8 of 20 brews');
    expect(
      syncProgressMessage({
        phase: 'uploading',
        scannedShots: 20,
        totalShots: 20,
        uploadedItems: 25,
        totalItems: 40,
      }),
    ).toBe('Uploading: 25 of 40 items');
    expect(syncProgressMessage({ phase: 'finishing' })).toBe('Finishing your first sync…');
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

  it.each([
    { code: 'SYNC_REAUTH_REQUIRED', message: 'Your MyBrewFolio connection needs to be renewed. Sign in again to resume syncing.' },
    { code: 'SYNC_DEVICE_REVOKED', message: 'This Sync installation was disconnected in MyBrewFolio. Sign in again to reconnect it.' },
  ])('offers a clear reconnect action for $code', ({ code, message }) => {
    render(<Setup status={{ ...status, connected: false, lastErrorCode: code }} refresh={vi.fn()} externalNotice={null} />);
    expect(screen.getByRole('alert').textContent).toContain(message);
    expect(screen.getByRole('button', { name: 'Sign in again' })).toBeTruthy();
  });

  it('does not suggest signing in again for a network error', () => {
    render(<Setup status={{ ...status, connected: false, lastErrorCode: 'MYBREWFOLIO_UNREACHABLE' }} refresh={vi.fn()} externalNotice={null} />);
    expect(screen.queryByRole('alert')).toBeNull();
    expect(screen.getByRole('button', { name: 'Connect MyBrewFolio' })).toBeTruthy();
  });

  it('renders normal controls and asks before disconnecting', () => {
    render(<Dashboard status={status} refresh={vi.fn()} onDisconnected={vi.fn()} disconnectRequestToken={0} />);
    expect(screen.getByText('10')).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Open Sync settings' })).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Disconnect account' }));
    expect(screen.getByText('Disconnect this computer?')).toBeTruthy();
  });

  it('turns off startup without showing an error when the returned state is disabled', async () => {
    invoke.mockImplementation(command => {
      if (command === 'get_autostart_status') return Promise.resolve({ enabled: true, requiresWindowsSettings: false, blockedByPolicy: false, migrationAvailable: false });
      if (command === 'set_autostart_enabled') return Promise.resolve({ enabled: false, requiresWindowsSettings: false, blockedByPolicy: false, migrationAvailable: false });
      return Promise.resolve(undefined);
    });
    render(<Dashboard status={status} refresh={vi.fn()} onDisconnected={vi.fn()} disconnectRequestToken={0} />);
    const toggle = screen.getByRole('checkbox', { name: 'Start Sync with this computer' });
    await vi.waitFor(() => expect(toggle.checked).toBe(true));
    fireEvent.click(toggle);
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('set_autostart_enabled', { enabled: false }));
    await vi.waitFor(() => expect(toggle.disabled).toBe(false));
    expect(toggle.checked).toBe(false);
    expect(screen.queryByText(/could not (enable|disable) startup|Windows did not enable startup/i)).toBeNull();
  });

  it.each([
    { initialEnabled: true, returnedEnabled: true, message: 'Could not disable startup for Sync.' },
    { initialEnabled: false, returnedEnabled: false, message: 'Could not enable startup for Sync.' },
  ])('shows an action-specific error when startup stays $returnedEnabled after a toggle', async ({ initialEnabled, returnedEnabled, message }) => {
    invoke.mockImplementation(command => {
      if (command === 'get_autostart_status') return Promise.resolve({ enabled: initialEnabled, requiresWindowsSettings: false, blockedByPolicy: false, migrationAvailable: false });
      if (command === 'set_autostart_enabled') return Promise.resolve({ enabled: returnedEnabled, requiresWindowsSettings: false, blockedByPolicy: false, migrationAvailable: false });
      return Promise.resolve(undefined);
    });
    render(<Dashboard status={status} refresh={vi.fn()} onDisconnected={vi.fn()} disconnectRequestToken={0} />);
    const toggle = screen.getByRole('checkbox', { name: 'Start Sync with this computer' });
    await vi.waitFor(() => expect(toggle.checked).toBe(initialEnabled));
    fireEvent.click(toggle);
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('set_autostart_enabled', { enabled: !initialEnabled }));
    await vi.waitFor(() => expect(screen.getByText(message)).toBeTruthy());
    expect(toggle.checked).toBe(returnedEnabled);
  });

  it.each([
    { requiresWindowsSettings: true, blockedByPolicy: false, message: 'Windows has disabled startup for Sync. Re-enable it in Settings > Apps > Startup.' },
    { requiresWindowsSettings: false, blockedByPolicy: true, message: 'Windows or your organization has blocked startup for Sync.' },
  ])('retains Windows-specific startup feedback when enabling is blocked', async ({ requiresWindowsSettings, blockedByPolicy, message }) => {
    invoke.mockImplementation(command => {
      if (command === 'get_autostart_status') return Promise.resolve({ enabled: false, requiresWindowsSettings: false, blockedByPolicy: false, migrationAvailable: false });
      if (command === 'set_autostart_enabled') return Promise.resolve({ enabled: false, requiresWindowsSettings, blockedByPolicy, migrationAvailable: false });
      return Promise.resolve(undefined);
    });
    render(<Dashboard status={status} refresh={vi.fn()} onDisconnected={vi.fn()} disconnectRequestToken={0} />);
    const toggle = screen.getByRole('checkbox', { name: 'Start Sync with this computer' });
    await vi.waitFor(() => expect(toggle.checked).toBe(false));
    fireEvent.click(toggle);
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('set_autostart_enabled', { enabled: true }));
    await vi.waitFor(() => expect(screen.getAllByText(message).length).toBeGreaterThan(0));
    expect(toggle.checked).toBe(false);
  });

  it('keeps Notes activation in MyBrewFolio even when requested remotely', async () => {
    render(<Dashboard status={{ ...status, notesSyncStatus: 'activation_pending', notesSyncTargetDeviceId: 'this-device' }} refresh={vi.fn()} onDisconnected={vi.fn()} disconnectRequestToken={0} />);
    fireEvent.click(screen.getByRole('button', { name: 'Open Sync settings' }));
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('open_mybrewfolio_page', { page: 'accountSync' }));
    expect(invoke).not.toHaveBeenCalledWith('begin_two_way_notes_activation');
    expect(screen.queryByRole('button', { name: 'Complete resync' })).toBeNull();
  });

  it('opens first-sync preferences in MyBrewFolio and preserves update and startup feedback', async () => {
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
    expect(screen.getByText(/Finish choosing your Sync preferences/)).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Open Sync settings' }));
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('open_mybrewfolio_page', { page: 'accountSync' }));
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
