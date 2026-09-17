// SPDX-License-Identifier: GPL-3.0-or-later

import { render } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { invoke } from '@tauri-apps/api/core';
import { getVersion } from '@tauri-apps/api/app';
import { listen } from '@tauri-apps/api/event';
import { getCurrent, onOpenUrl } from '@tauri-apps/plugin-deep-link';
import './style.css';

export const FIRST_SYNCHRONIZATION_MESSAGE =
  'Your first sync may take a while, depending on your history. You can leave this page and come back later. Keep the Sync app or Docker container running.';

export function firstSynchronizationInProgress(status) {
  return Boolean(status?.connected && status?.syncing && !status?.lastSyncAt && !status?.lastError);
}

const initialStatus = {
  connected: false,
  machineHost: 'gaggimate.local',
  machineReachable: false,
  syncing: false,
  lastSyncAt: null,
  lastError: null,
  profiles: 0,
  shots: 0,
  notes: 0,
  conflicts: 0,
  suppressed: 0,
  initialSyncConfigured: false,
  duplicatePolicy: 'reuse_matching',
  notesSyncStatus: 'one_way',
  notesSyncTargetDeviceId: null,
  notesSyncWriterDeviceId: null,
  thisDeviceId: null,
  notesSyncIntroSeen: false,
  noteBackups: [],
  issues: [],
};

export function formatDate(value) {
  if (!value) return 'Not synced yet';
  const date = new Date(value);
  return Number.isFinite(date.getTime()) ? date.toLocaleString() : 'Not synced yet';
}

export function statusTone(activeSyncActivity, message, messageTone, engineError) {
  if (activeSyncActivity) return 'working';
  if (message) return messageTone;
  if (engineError) return 'error';
  return 'info';
}

function SyncSpinner() {
  return <span className="sync-spinner" aria-hidden="true" />;
}

function ActionLabel({ active, activeText, children }) {
  return (
    <span className="action-label">
      {active ? <SyncSpinner /> : null}
      {active ? activeText : children}
    </span>
  );
}

function ExternalLink({ page, children, className = '' }) {
  return (
    <button
      type="button"
      className={`text-link ${className}`.trim()}
      onClick={() => invoke('open_mybrewfolio_page', { page }).catch(() => {})}
    >
      {children}
    </button>
  );
}

function AppFooter() {
  return (
    <footer className="app-footer" aria-label="MyBrewFolio links">
      <ExternalLink page="syncHelp">Support</ExternalLink>
      <span aria-hidden="true">·</span>
      <ExternalLink page="privacy">Privacy</ExternalLink>
    </footer>
  );
}

function StatusPill({ status }) {
  const kind = status.connected ? 'ok' : 'idle';
  return <span className={`status status-${kind}`}>{status.connected ? 'Connected' : 'Not connected'}</span>;
}

const syncActivityLabels = {
  sync: 'Synchronizing with GaggiMate…',
  'first-sync': 'Running the first synchronization…',
  retry: 'Retrying failed Sync items…',
  'resync-preview': 'Reading the complete GaggiMate library…',
  'resync-apply': 'Applying the complete resync…',
  'notes-activation': 'Backing up GaggiMate Notes…',
  'notes-backup': 'Backing up GaggiMate Notes…',
  'notes-write': 'Enabling two-way Notes Sync…',
  'notes-restore': 'Restoring GaggiMate Notes…',
};

function visibleDashboardStatus(activeSyncActivity, status, message, engineError) {
  if (!activeSyncActivity) return message || engineError;
  if (firstSynchronizationInProgress(status)) return FIRST_SYNCHRONIZATION_MESSAGE;
  return syncActivityLabels[activeSyncActivity];
}

function UpdateSettings({ updateStatus, showUpdateDialog, busy, checkForUpdates, restartAfterUpdate, appVersion }) {
  let updateAction = <button type="button" className="secondary inline-action" disabled={busy} onClick={checkForUpdates}>Check for updates</button>;
  if (updateStatus.kind === 'installed') {
    updateAction = <>
      <p className="muted">Update {updateStatus.version} is installed.</p>
      {!showUpdateDialog ? (
        <button type="button" className="primary inline-action" onClick={restartAfterUpdate}>
          {updateStatus.restartRequested ? 'Restart scheduled' : 'Restart Sync'}
        </button>
      ) : null}
    </>;
  } else if (updateStatus.kind === 'storeManaged') {
    updateAction = <p className="muted">Updates are managed by Microsoft Store.</p>;
  }
  return (
    <section className="card settings">
      <h3>Updates</h3>
      {updateStatus.kind === 'available' ? <output className="update-available" aria-live="polite">Update {updateStatus.version} is available.</output> : null}
      {updateAction}
      <p className="muted app-version">Installed version {appVersion || '…'}</p>
    </section>
  );
}

export function Setup({ status, refresh, externalNotice }) {
  const [host, setHost] = useState(status.machineHost || 'gaggimate.local');
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState('');

  const connect = async () => {
    setBusy(true);
    setMessage('');
    try {
      await invoke('set_machine_host', { host });
      await invoke('begin_oauth');
      setMessage('Confirm the connection in your browser. This window will continue automatically.');
    } catch (error) {
      setMessage(String(error));
    } finally {
      setBusy(false);
      refresh();
    }
  };

  return (
    <main className="shell setup">
      <header className="brand-row">
        <div className="mark">my<br />brew<br />folio</div>
        <span className="alpha-label">ALPHA</span>
      </header>
      <section className="hero">
        <p className="eyebrow">MYBREWFOLIO SYNC</p>
        <h1>Your smart coffee machine library, available everywhere.</h1>
        <p>Shots, profiles, and Notes are copied to your private MyBrewFolio library. If you enable Two-way Notes Sync, only Notes can also be updated on your machine.</p>
        <ExternalLink page="syncHelp" className="setup-help">Sync help</ExternalLink>
      </section>
      <ol className="steps">
        <li className="done"><span>1</span><div><strong>Install Sync</strong><small>Done on this computer</small></div></li>
        <li><span>2</span><div><strong>Connect MyBrewFolio</strong><small>Confirm sign-in in your browser</small></div></li>
        <li><span>3</span><div><strong>Confirm GaggiMate</strong><small>Usually found as gaggimate.local</small></div></li>
      </ol>
      <label className="field">
        <span>GaggiMate hostname or local IP</span>
        <input value={host} onInput={event => setHost(event.currentTarget.value)} placeholder="gaggimate.local" />
      </label>
      <button type="button" className="primary" disabled={busy} onClick={connect}>{busy ? 'Opening browser…' : 'Connect MyBrewFolio'}</button>
      {message ? <p className="message" aria-live="polite">{message}</p> : null}
      {!message && externalNotice ? (
        <div className="message disconnect-notice" aria-live="polite">
          <span>{externalNotice.message}</span>
          {externalNotice.page ? <ExternalLink page={externalNotice.page}>{externalNotice.action}</ExternalLink> : null}
        </div>
      ) : null}
      <p className="privacy">The local address stays on this computer. Only the library content you synchronize is sent to MyBrewFolio.</p>
      <AppFooter />
    </main>
  );
}

export function Dashboard({ status, refresh, onDisconnected, disconnectRequestToken }) {
  const [autostart, setAutostart] = useState(true);
  const [autostartStatus, setAutostartStatus] = useState({
    enabled: true,
    requiresWindowsSettings: false,
    blockedByPolicy: false,
    migrationAvailable: false,
  });
  const [hideAppIcon, setHideAppIconState] = useState(false);
  const [host, setHost] = useState(status.machineHost);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState('');
  const [messageTone, setMessageTone] = useState('success');
  const [acknowledgedLastError, setAcknowledgedLastError] = useState('');
  const [appVersion, setAppVersion] = useState('');
  const [syncActivity, setSyncActivity] = useState('');
  const [updateStatus, setUpdateStatus] = useState({ kind: 'unknown' });
  const [showUpdateDialog, setShowUpdateDialog] = useState(false);
  const [confirmDisconnect, setConfirmDisconnect] = useState(false);

  const showStatusMessage = (text, tone = 'success') => {
    if (tone === 'success' && status.lastError) setAcknowledgedLastError(status.lastError);
    setMessageTone(tone);
    setMessage(text);
  };

  useEffect(() => {
    if (!message || messageTone !== 'success') return undefined;
    const timer = globalThis.setTimeout(() => setMessage(''), 4_000);
    return () => globalThis.clearTimeout(timer);
  }, [message, messageTone]);

  useEffect(() => {
    if (!status.lastError) setAcknowledgedLastError('');
  }, [status.lastError]);

  useEffect(() => {
    invoke('get_autostart_status')
      .then(result => {
        setAutostartStatus(result);
        setAutostart(result.enabled);
      })
      .catch(() => {
        setAutostart(false);
        setAutostartStatus(current => ({ ...current, enabled: false }));
      });
    invoke('get_hide_app_icon').then(setHideAppIconState).catch(() => setHideAppIconState(false));
    getVersion().then(setAppVersion).catch(() => setAppVersion('Unknown'));
    invoke('get_update_status')
      .then(result => {
        const next = result || { kind: 'unknown' };
        setUpdateStatus(next);
        setShowUpdateDialog(next.kind === 'available' && next.promptPending);
      })
      .catch(() => {
        // A background update check must never interrupt synchronization.
      });
  }, []);
  useEffect(() => {
    if (disconnectRequestToken > 0) setConfirmDisconnect(true);
  }, [disconnectRequestToken]);
  const syncNow = async () => {
    setBusy(true);
    setSyncActivity('sync');
    setMessage('');
    try {
      await invoke('sync_now');
      showStatusMessage('Synchronization completed.');
    } catch (error) {
      showStatusMessage(String(error), 'error');
    } finally {
      setBusy(false);
      setSyncActivity('');
      refresh();
    }
  };

  const saveHost = async () => {
    setBusy(true);
    try {
      await invoke('set_machine_host', { host });
      showStatusMessage('Machine address saved.');
      refresh();
    } catch (error) {
      showStatusMessage(String(error), 'error');
    } finally {
      setBusy(false);
    }
  };

  const toggleAutostart = async event => {
    const checked = event.currentTarget.checked;
    setAutostart(checked);
    setBusy(true);
    try {
      const result = await invoke('set_autostart_enabled', { enabled: checked });
      setAutostartStatus(result);
      setAutostart(result.enabled);
      if (!result.enabled) {
        if (result.requiresWindowsSettings) {
          showStatusMessage('Windows has disabled startup for Sync. Re-enable it in Settings > Apps > Startup.');
        } else if (result.blockedByPolicy) {
          showStatusMessage('Windows or your organization has blocked startup for Sync.', 'error');
        } else {
          showStatusMessage('Windows did not enable startup for Sync.', 'error');
        }
      }
    } catch (error) {
      setAutostart(!checked);
      showStatusMessage(String(error), 'error');
    } finally {
      setBusy(false);
    }
  };

  const toggleAppIcon = async event => {
    const hidden = event.currentTarget.checked;
    setHideAppIconState(hidden);
    setBusy(true);
    try {
      await invoke('set_hide_app_icon', { hidden });
      showStatusMessage(hidden
        ? 'App icon hidden. Use the menu bar or tray icon to open Sync.'
        : 'App icon is visible again.');
    } catch (error) {
      setHideAppIconState(!hidden);
      showStatusMessage(String(error), 'error');
    } finally {
      setBusy(false);
    }
  };

  const disconnect = async () => {
    setBusy(true);
    try {
      const result = await invoke('disconnect_account');
      setConfirmDisconnect(false);
      await onDisconnected(result);
    } catch (error) {
      showStatusMessage(String(error), 'error');
    } finally {
      setBusy(false);
    }
  };

  useEffect(() => {
    let unlisten;
    listen('update-status-changed', event => {
      const next = event.payload || { kind: 'unknown' };
      if (next.kind === 'error') {
        showStatusMessage(next.message, 'error');
        return;
      }
      setUpdateStatus(next);
      if (next.kind === 'available' && next.promptPending) setShowUpdateDialog(true);
    }).then(stop => { unlisten = stop; });
    return () => unlisten?.();
  }, []);

  const checkForUpdates = async () => {
    setBusy(true);
    showStatusMessage('Checking for updates…', 'info');
    try {
      const next = await invoke('check_update');
      if (next.kind === 'error') {
        showStatusMessage(next.message, 'error');
        return;
      }
      setUpdateStatus(next);
      setShowUpdateDialog(next.kind === 'available' && next.promptPending);
      if (next.kind === 'upToDate') showStatusMessage('MyBrewFolio Sync is up to date.');
      else if (next.kind === 'storeManaged') showStatusMessage('Updates are managed by Microsoft Store.');
      else if (next.kind === 'notConfigured') showStatusMessage('Automatic updates are not available in this development build.');
    } catch {
      showStatusMessage('Unable to check for updates. Sync will try again later.', 'error');
    } finally {
      setBusy(false);
    }
  };

  const installUpdate = async () => {
    setBusy(true);
    try {
      const next = await invoke('install_update');
      if (next.kind === 'error') {
        showStatusMessage(next.message, 'error');
        return;
      }
      setUpdateStatus(next);
      if (next.kind === 'installed') setShowUpdateDialog(true);
      else if (next.kind === 'upToDate') {
        setShowUpdateDialog(false);
        showStatusMessage('MyBrewFolio Sync is up to date.');
      }
    } catch {
      showStatusMessage('Unable to install the update. Please try again later.', 'error');
    } finally {
      setBusy(false);
    }
  };

  const laterUpdate = async () => {
    try {
      const next = await invoke('dismiss_update');
      setUpdateStatus(next);
      setShowUpdateDialog(false);
    } catch {
      showStatusMessage('Unable to postpone the update reminder. Please try again.', 'error');
    }
  };

  const restartAfterUpdate = async () => {
    try {
      const next = await invoke('restart_after_update');
      setUpdateStatus(next);
      if (next.restartRequested) {
        if (next.restartWaitingForSync) {
          showStatusMessage('Restarting after the current synchronization finishes.', 'info');
        }
      }
    } catch {
      showStatusMessage('Unable to restart Sync. Please restart it manually.', 'error');
    }
  };

  const activeSyncActivity = syncActivity || (status.syncing ? 'sync' : '');
  const engineError = status.lastError && status.lastError !== acknowledgedLastError
    ? status.lastError
    : '';
  const visibleStatusMessage = visibleDashboardStatus(activeSyncActivity, status, message, engineError);
  const visibleStatusTone = statusTone(activeSyncActivity, message, messageTone, engineError);
  return (
    <DashboardContent
      status={status}
      visibleStatusMessage={visibleStatusMessage}
      visibleStatusTone={visibleStatusTone}
      busy={busy}
      activeSyncActivity={activeSyncActivity}
      syncNow={syncNow}
      host={host}
      setHost={setHost}
      saveHost={saveHost}
      updateStatus={updateStatus}
      showUpdateDialog={showUpdateDialog}
      checkForUpdates={checkForUpdates}
      restartAfterUpdate={restartAfterUpdate}
      appVersion={appVersion}
      autostart={autostart}
      autostartStatus={autostartStatus}
      hideAppIcon={hideAppIcon}
      toggleAutostart={toggleAutostart}
      toggleAppIcon={toggleAppIcon}
      confirmDisconnect={confirmDisconnect}
      setConfirmDisconnect={setConfirmDisconnect}
      disconnect={disconnect}
      laterUpdate={laterUpdate}
      installUpdate={installUpdate}
    />
  );
}

function DashboardContent({
  status, visibleStatusMessage, visibleStatusTone, busy, activeSyncActivity, syncNow,
  host, setHost, saveHost, updateStatus, showUpdateDialog, checkForUpdates,
  restartAfterUpdate, appVersion, autostart, autostartStatus, hideAppIcon,
  toggleAutostart, toggleAppIcon, confirmDisconnect, setConfirmDisconnect,
  disconnect, laterUpdate, installUpdate,
}) {
  return (
    <main className="shell">
      <header className="brand-row dashboard-header">
        <div><div className="mark compact">my<br />brew<br />folio</div><h1>Sync</h1></div>
        <StatusPill status={status} />
      </header>
      {visibleStatusMessage ? (
        <output className={`central-status central-status-${visibleStatusTone}`} aria-live="polite">
          <strong>{visibleStatusMessage}</strong>
        </output>
      ) : null}
      <section className="overview card">
        <div><small>Last successful sync</small><strong>{formatDate(status.lastSyncAt)}</strong></div>
        <button type="button" className="primary compact-button" disabled={busy || status.syncing} onClick={syncNow}>
          <ActionLabel active={activeSyncActivity === 'sync'} activeText="Syncing…">Sync now</ActionLabel>
        </button>
      </section>
      <section className="counts">
        <article className="card"><strong>{status.shots}</strong><span>Shots</span></article>
        <article className="card"><strong>{status.profiles}</strong><span>Profiles</span></article>
        <article className="card"><strong>{status.notes}</strong><span>Notes</span></article>
      </section>
      <section className="card settings">
        <h3>Manage Sync in MyBrewFolio</h3>
        <p className="muted">Manage matching preferences, Notes, backups, conflicts and complete resync in your account.</p>
        {!status.initialSyncConfigured ? <p>Finish choosing your Sync preferences in MyBrewFolio to start importing.</p> : null}
        <ExternalLink page="accountSync" className="primary">Open Sync settings</ExternalLink>
      </section>
      <section className="card settings">
        <h3>Local connection</h3>
        <div className="inline-field"><input aria-label="GaggiMate hostname or local IP" value={host} onInput={event => setHost(event.currentTarget.value)} /><button type="button" onClick={saveHost} disabled={busy}>Save</button></div>
        <p className="muted">The machine address stays on this computer.</p>
        {status.issues?.length ? <p className="muted">{status.issues.length} local items need attention. Review and retry them in MyBrewFolio.</p> : null}
      </section>
      <h2 className="section-title">App settings</h2>
      <UpdateSettings updateStatus={updateStatus} showUpdateDialog={showUpdateDialog} busy={busy} checkForUpdates={checkForUpdates} restartAfterUpdate={restartAfterUpdate} appVersion={appVersion} />
      <section className="card settings background-app-settings">
        <h3>Background app</h3>
        <label className="toggle"><input type="checkbox" checked={autostart} onChange={toggleAutostart} disabled={busy || autostartStatus.requiresWindowsSettings || autostartStatus.blockedByPolicy} /><span>Start Sync with this computer</span></label>
        {autostartStatus.migrationAvailable ? (
          <p className="muted app-visibility-help">Windows needs a one-time confirmation to keep your existing startup choice. Turn this on and accept the Windows prompt.</p>
        ) : null}
        {autostartStatus.requiresWindowsSettings ? (
          <p className="muted app-visibility-help">Windows has disabled startup for Sync. Re-enable it in Settings &gt; Apps &gt; Startup.</p>
        ) : null}
        {autostartStatus.blockedByPolicy ? (
          <p className="muted app-visibility-help">Startup for Sync is disabled by Windows or your organization.</p>
        ) : null}
        <label className="toggle"><input type="checkbox" checked={hideAppIcon} onChange={toggleAppIcon} disabled={busy} /><span>Hide app icon from Dock or taskbar</span></label>
        <p className="muted app-visibility-help">The menu bar or tray icon stays available so you can reopen Sync at any time.</p>
      </section>
      <section className="card account-action">
        <h3>Account</h3>
        <p className="muted">This installation is connected to your private MyBrewFolio library.</p>
        {confirmDisconnect ? (
          <div className="disconnect-confirm" role="alertdialog" aria-labelledby="disconnect-title">
            <strong id="disconnect-title">Disconnect this computer?</strong>
            <div>
              <button type="button" className="secondary compact-button" disabled={busy} onClick={() => setConfirmDisconnect(false)}>Cancel</button>
              <button type="button" className="primary compact-button" disabled={busy} onClick={disconnect}>Disconnect</button>
            </div>
          </div>
        ) : (
          <button type="button" className="secondary" disabled={busy} onClick={() => setConfirmDisconnect(true)}>Disconnect account</button>
        )}
      </section>
      {showUpdateDialog && updateStatus.kind === 'available' ? (
        <section className="card modal-card" role="alertdialog" aria-labelledby="update-available-title">
          <div className="modal-title-row">
            <h2 id="update-available-title">Update available</h2>
          </div>
          <p>MyBrewFolio Sync {updateStatus.version} is ready to install.</p>
          <div className="dialog-actions">
            <button type="button" className="secondary compact-button" disabled={busy} onClick={laterUpdate}>Later</button>
            <button type="button" className="primary compact-button" disabled={busy} onClick={installUpdate}>
              Install update
            </button>
          </div>
        </section>
      ) : null}
      {showUpdateDialog && updateStatus.kind === 'installed' ? (
        <section className="card modal-card" role="alertdialog" aria-labelledby="update-installed-title">
          <div className="modal-title-row">
            <h2 id="update-installed-title">Update installed</h2>
          </div>
          <p>MyBrewFolio Sync {updateStatus.version} is installed. Restart Sync to use the new version.</p>
          {updateStatus.restartWaitingForSync ? (
            <p className="muted">Restarting after the current synchronization finishes.</p>
          ) : null}
          <div className="dialog-actions">
            <button type="button" className="primary compact-button" onClick={restartAfterUpdate}>
              {updateStatus.restartRequested ? 'Restart scheduled' : 'Restart Sync'}
            </button>
          </div>
        </section>
      ) : null}
      <AppFooter />
    </main>
  );
}

export function App() {
  const [status, setStatus] = useState(initialStatus);
  const [loading, setLoading] = useState(true);
  const [oauthError, setOauthError] = useState('');
  const [disconnectNotice, setDisconnectNotice] = useState(null);
  const [disconnectRequestToken, setDisconnectRequestToken] = useState(0);
  const refresh = async () => {
    try { setStatus(await invoke('get_status')); } finally { setLoading(false); }
  };

  useEffect(() => {
    invoke('frontend_ready').catch(() => {});
    refresh();
    const poll = setInterval(refresh, 5000);
    let unlistenDeepLink;
    let unlistenStatus;
    let unlistenSync;
    let unlistenDisconnect;
    const handleUrls = urls => {
      const callback = urls?.find(url => url.startsWith('mybrewfolio-sync://oauth/callback'));
      if (callback) {
        setOauthError('');
        setDisconnectNotice(null);
        invoke('complete_oauth', { callbackUrl: callback })
          .then(refresh)
          .catch(error => setOauthError(`MyBrewFolio could not finish connecting this installation: ${String(error)}`));
      }
    };
    getCurrent().then(handleUrls).catch(() => {});
    onOpenUrl(handleUrls).then(unlisten => { unlistenDeepLink = unlisten; });
    listen('sync-status-changed', refresh).then(unlisten => { unlistenStatus = unlisten; });
    listen('sync-requested', () => invoke('sync_now').finally(refresh)).then(unlisten => { unlistenSync = unlisten; });
    listen('disconnect-confirmation-requested', () => setDisconnectRequestToken(value => value + 1))
      .then(unlisten => { unlistenDisconnect = unlisten; });
    return () => {
      clearInterval(poll);
      unlistenDeepLink?.();
      unlistenStatus?.();
      unlistenSync?.();
      unlistenDisconnect?.();
    };
  }, []);

  const handleDisconnected = async result => {
    if (!result?.credentialsRemoved) {
      setDisconnectNotice({
        message: 'Disconnected, but the stored sign-in could not be removed.',
        page: 'syncHelp',
        action: 'Get help',
      });
    } else if (!result?.serverRevoked) {
      setDisconnectNotice({
        message: 'Disconnected here. Revoke the installation at MyBrewFolio Account → Sync.',
        page: 'accountSync',
        action: 'Open Account → Sync',
      });
    } else {
      setDisconnectNotice({ message: 'This computer was disconnected.' });
    }
    await refresh();
  };

  if (loading) return <main className="shell loading">Loading MyBrewFolio Sync…</main>;
  return status.connected
    ? <Dashboard status={status} refresh={refresh} onDisconnected={handleDisconnected} disconnectRequestToken={disconnectRequestToken} />
    : <Setup status={status} refresh={refresh} externalNotice={oauthError ? { message: oauthError } : disconnectNotice} />;
}

const appRoot = document.getElementById('app');
if (appRoot) render(<App />, appRoot);
