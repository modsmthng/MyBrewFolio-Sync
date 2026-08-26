// SPDX-License-Identifier: GPL-3.0-or-later

import { render } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { invoke } from '@tauri-apps/api/core';
import { getVersion } from '@tauri-apps/api/app';
import { listen } from '@tauri-apps/api/event';
import { getCurrent, onOpenUrl } from '@tauri-apps/plugin-deep-link';
import './style.css';

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

const REUSE_MATCHING_SHOTS_EXPLANATION =
  'Matches use the GaggiMate shot ID and recording time. Existing MyBrewFolio changes are not silently overwritten.';

const COMPLETE_RESYNC_EXPLANATION =
  'Reads the complete GaggiMate library again. Shots and profiles deleted from MyBrewFolio can be restored if they are still on the machine. You review the changes before anything is applied. Complete resync itself does not write to your GaggiMate.';

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

export function activationDecisions(preview) {
  return Object.fromEntries(
    (preview.items || [])
      .filter(item => item.differs)
      .map(item => [item.sourceKey, 'mybrewfolio']),
  );
}

export function resyncDecisions(preview) {
  return {
    restoreIds: (preview.restoreItems || []).map(item => item.id),
    duplicateDecisions: (preview.duplicates || []).map(item => ({
      mappingId: item.mapping_id,
      keepShotId: item.keep_shot_id,
      removeShotId: item.remove_shot_id,
      selected: true,
      notesResolution: item.notes_conflict ? '' : undefined,
    })),
  };
}

function SyncSpinner() {
  return <span className="sync-spinner" aria-hidden="true" />;
}

function ResyncPreview({
  resync,
  restoreIds,
  setRestoreIds,
  duplicateDecisions,
  setDuplicateDecisions,
  confirmingResync,
  setConfirmingResync,
  busy,
  syncActivity,
  applyResync,
  onCancel,
}) {
  return (
    <section className="card resync-preview" role="dialog" aria-labelledby="resync-title">
      <h2 id="resync-title">Complete resync preview</h2>
      <p>
        {resync.restoreItems?.length || 0} deleted machine items can be restored.{' '}
        {resync.duplicates?.length || 0} duplicate shots can be merged.{' '}
        {resync.ambiguousDuplicates?.length || 0} ambiguous matches will remain unchanged.
      </p>
      {(resync.restoreItems || []).length ? <fieldset>
        <legend>Restore from GaggiMate</legend>
        {resync.restoreItems.map(item => <label className="toggle" key={item.id}>
          <input type="checkbox" checked={restoreIds.includes(item.id)} onChange={event => setRestoreIds(current => event.currentTarget.checked ? [...current, item.id] : current.filter(id => id !== item.id))} />
          <span>{item.kind}: {item.sourceKey}</span>
        </label>)}
      </fieldset> : null}
      {(resync.duplicates || []).length ? <fieldset>
        <legend>Duplicate shots</legend>
        <div className="bulk-actions">
          <button type="button" className="secondary inline-action" onClick={() => setDuplicateDecisions(current => current.map(item => ({ ...item, selected: true })))}>Select all</button>
          <button type="button" className="secondary inline-action" onClick={() => setDuplicateDecisions(current => current.map(item => item.notesResolution === '' ? { ...item, notesResolution: 'mybrewfolio' } : item))}>Keep MyBrewFolio notes for all</button>
          <button type="button" className="secondary inline-action" onClick={() => setDuplicateDecisions(current => current.map(item => item.notesResolution === '' ? { ...item, notesResolution: 'gaggimate' } : item))}>Use GaggiMate notes for all</button>
        </div>
        {resync.duplicates.map((item, index) => <div className="duplicate-row" key={item.mapping_id}>
          <label className="toggle">
            <input type="checkbox" checked={duplicateDecisions[index]?.selected} onChange={event => setDuplicateDecisions(current => current.map((decision, position) => position === index ? { ...decision, selected: event.currentTarget.checked } : decision))} />
            <span>Keep “{item.keep_name}” and remove Sync copy “{item.remove_name}”</span>
          </label>
          {item.notes_conflict ? <select aria-label={`Notes resolution for ${item.mapped_name}`} value={duplicateDecisions[index]?.notesResolution || ''} onChange={event => setDuplicateDecisions(current => current.map((decision, position) => position === index ? { ...decision, notesResolution: event.currentTarget.value } : decision))}>
            <option value="">Choose notes…</option>
            <option value="mybrewfolio">Keep MyBrewFolio notes</option>
            <option value="gaggimate">Use GaggiMate notes</option>
          </select> : null}
        </div>)}
      </fieldset> : null}
      {(resync.ambiguousDuplicates || []).length ? <fieldset>
        <legend>Unresolved matches</legend>
        <p className="muted">These shots have more than one possible match. No copy will be merged or deleted.</p>
        {resync.ambiguousDuplicates.map(item => (
          <div className="duplicate-row" key={item.mapping_id}>
            <strong>{item.mapped_name}</strong>
            <p className="muted">{item.candidate_count} possible MyBrewFolio matches</p>
          </div>
        ))}
      </fieldset> : null}
      <div className="dialog-actions">
        {confirmingResync ? (
          <p className="message" role="alert">
            Confirm restoring {restoreIds.length} selected machine items
            {duplicateDecisions.filter(item => item.selected).length
              ? ` and merging ${duplicateDecisions.filter(item => item.selected).length} selected duplicate shots`
              : ''}.
          </p>
        ) : null}
        <button type="button" className="secondary inline-action" disabled={busy} onClick={() => {
          if (confirmingResync) {
            setConfirmingResync(false);
          } else {
            // The parent owns the preview lifecycle; a cancelled preview is
            // represented by the same null state as a completed apply.
            onCancel();
          }
        }}>{confirmingResync ? 'Back' : 'Cancel'}</button>
        <button type="button" className="primary compact-button" disabled={busy} onClick={applyResync}>
          <ActionLabel active={syncActivity === 'resync-apply'} activeText="Applying resync…">
            {confirmingResync ? 'Confirm complete resync' : 'Apply complete resync'}
          </ActionLabel>
        </button>
      </div>
    </section>
  );
}

function ActionLabel({ active, activeText, children }) {
  return (
    <span className="action-label">
      {active ? <SyncSpinner /> : null}
      {active ? activeText : children}
    </span>
  );
}

function InfoIcon() {
  return (
    <svg aria-hidden="true" viewBox="0 0 24 24">
      <circle cx="12" cy="12" r="9" />
      <path d="M12 10.75v5.5M12 7.75h.01" />
    </svg>
  );
}

function DownArrowIcon() {
  return (
    <svg aria-hidden="true" viewBox="0 0 24 24">
      <path d="M12 4v15M6.5 13.5 12 19l5.5-5.5" />
    </svg>
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
  const [reuseMatching, setReuseMatching] = useState(status.duplicatePolicy !== 'import_all');
  const [policyDirty, setPolicyDirty] = useState(false);
  const [appVersion, setAppVersion] = useState('');
  const [resync, setResync] = useState(null);
  const [restoreIds, setRestoreIds] = useState([]);
  const [duplicateDecisions, setDuplicateDecisions] = useState([]);
  const [syncActivity, setSyncActivity] = useState('');
  const [confirmingResync, setConfirmingResync] = useState(false);
  const [showMatchingInfo, setShowMatchingInfo] = useState(false);
  const [showCompleteResyncInfo, setShowCompleteResyncInfo] = useState(false);
  const [showNotesSyncInfo, setShowNotesSyncInfo] = useState(false);
  const [showNotesIntroInfo, setShowNotesIntroInfo] = useState(false);
  const [availableUpdate, setAvailableUpdate] = useState('');
  const [confirmDisconnect, setConfirmDisconnect] = useState(false);
  const [notesIntroOpen, setNotesIntroOpen] = useState(false);
  const [notesActivation, setNotesActivation] = useState(null);
  const [notesDecisions, setNotesDecisions] = useState({});
  const [restorePreview, setRestorePreview] = useState(null);
  const [restoreKeys, setRestoreKeys] = useState([]);
  const [autoActivationStarted, setAutoActivationStarted] = useState(false);
  const notesConfirmationRef = useRef(null);

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
    invoke('check_update')
      .then(result => {
        if (result.startsWith('available:')) {
          setAvailableUpdate(result.slice('available:'.length));
        }
      })
      .catch(() => {
        // A background update check must never interrupt synchronization.
      });
  }, []);
  useEffect(() => {
    if (disconnectRequestToken > 0) setConfirmDisconnect(true);
  }, [disconnectRequestToken]);
  useEffect(() => {
    if (!policyDirty) setReuseMatching(status.duplicatePolicy !== 'import_all');
  }, [status.duplicatePolicy, policyDirty]);
  useEffect(() => {
    if (status.initialSyncConfigured && !status.notesSyncIntroSeen && status.notesSyncStatus === 'one_way') {
      setNotesIntroOpen(true);
    }
  }, [status.initialSyncConfigured, status.notesSyncIntroSeen, status.notesSyncStatus]);

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

  const update = async () => {
    setBusy(true);
    showStatusMessage('Checking for updates…', 'info');
    try {
      const result = await invoke('install_update');
      const messages = {
        installed: 'The update was installed. Restart Sync to use the new version.',
        'up-to-date': 'MyBrewFolio Sync is up to date.',
        'store-managed': 'Updates are managed by Microsoft Store.',
        'not-configured': 'Automatic updates are not available in this development build.',
      };
      showStatusMessage(messages[result] || 'Update check completed.');
      if (result === 'installed' || result === 'up-to-date') setAvailableUpdate('');
    } catch (error) {
      showStatusMessage(String(error), 'error');
    } finally {
      setBusy(false);
    }
  };

  const configure = async () => {
    setBusy(true);
    setSyncActivity(status.initialSyncConfigured ? '' : 'first-sync');
    setMessage('');
    try {
      await invoke('configure_sync', { reuseMatching });
      setPolicyDirty(false);
      showStatusMessage(status.initialSyncConfigured
        ? 'Matching preference saved.'
        : 'Sync preferences saved and the first synchronization completed.');
    } catch (error) {
      showStatusMessage(String(error), 'error');
    } finally {
      setBusy(false);
      setSyncActivity('');
      refresh();
    }
  };

  const retryFailures = async () => {
    setBusy(true);
    setSyncActivity('retry');
    try {
      await invoke('retry_failed_items');
      showStatusMessage('Items not synchronized were checked again.');
    } catch (error) {
      showStatusMessage(String(error), 'error');
    } finally {
      setBusy(false);
      setSyncActivity('');
      refresh();
    }
  };

  const dismissNotesIntro = async () => {
    setNotesIntroOpen(false);
    await invoke('dismiss_notes_sync_intro');
    refresh();
  };

  const beginNotesActivation = async () => {
    setAutoActivationStarted(true);
    setBusy(true);
    setSyncActivity('notes-activation');
    setMessage('');
    try {
      const preview = await invoke('begin_two_way_notes_activation');
      setNotesActivation(preview);
      setNotesDecisions(activationDecisions(preview));
      setNotesIntroOpen(false);
      setMessage('');
    } catch (error) {
      showStatusMessage(String(error), 'error');
    } finally {
      setBusy(false);
      setSyncActivity('');
      refresh();
    }
  };

  useEffect(() => {
    const assignedHere = status.notesSyncStatus === 'activation_pending'
      && status.notesSyncTargetDeviceId
      && status.notesSyncTargetDeviceId === status.thisDeviceId;
    if (assignedHere && !autoActivationStarted && !notesActivation) {
      setAutoActivationStarted(true);
      beginNotesActivation();
    }
    if (!assignedHere) setAutoActivationStarted(false);
  }, [status.notesSyncStatus, status.notesSyncTargetDeviceId, status.thisDeviceId, autoActivationStarted, notesActivation]);

  const confirmNotesActivation = async () => {
    setBusy(true);
    setSyncActivity('notes-write');
    try {
      const decisions = Object.entries(notesDecisions).map(([sourceKey, resolution]) => ({ sourceKey, resolution }));
      await invoke('activate_two_way_notes', { backupId: notesActivation.backupId, decisions });
      setNotesActivation(null);
      showStatusMessage('Two-way Notes Sync is active.');
      await invoke('sync_now');
    } catch (error) {
      showStatusMessage(String(error), 'error');
    } finally {
      setBusy(false);
      setSyncActivity('');
      refresh();
    }
  };

  const disableNotesSync = async () => {
    setBusy(true);
    try {
      await invoke('disable_two_way_notes');
      showStatusMessage('Two-way Notes Sync is off.');
    } catch (error) {
      showStatusMessage(String(error), 'error');
    } finally {
      setBusy(false);
      refresh();
    }
  };

  const createNotesBackup = async () => {
    setBusy(true);
    setSyncActivity('notes-backup');
    try {
      await invoke('create_latest_notes_backup');
      showStatusMessage('Latest Backup created.');
    } catch (error) {
      showStatusMessage(String(error), 'error');
    } finally {
      setBusy(false);
      setSyncActivity('');
      refresh();
    }
  };

  const previewRestore = async backup => {
    setBusy(true);
    try {
      const preview = await invoke('preview_notes_restore', { backupId: backup.id });
      const available = (preview.items || []).filter(item => item.available);
      setRestorePreview({ ...preview, backup });
      setRestoreKeys(available.map(item => item.source_key || item.sourceKey));
    } catch (error) {
      showStatusMessage(String(error), 'error');
    } finally {
      setBusy(false);
    }
  };

  const restoreNotes = async () => {
    setBusy(true);
    setSyncActivity('notes-restore');
    try {
      const result = await invoke('restore_notes_backup', { backupId: restorePreview.backup.id, sourceKeys: restoreKeys });
      setRestorePreview(null);
      showStatusMessage(`Notes restore finished. ${result.applied} restored, ${result.skipped} skipped.`);
    } catch (error) {
      showStatusMessage(String(error), 'error');
    } finally {
      setBusy(false);
      setSyncActivity('');
      refresh();
    }
  };

  const previewResync = async () => {
    setBusy(true);
    setSyncActivity('resync-preview');
    setMessage('');
    try {
      const preview = await invoke('preview_complete_resync');
      setResync(preview);
      const decisions = resyncDecisions(preview);
      setRestoreIds(decisions.restoreIds);
      setDuplicateDecisions(decisions.duplicateDecisions);
      setConfirmingResync(false);
      setMessage('');
    } catch (error) {
      showStatusMessage(String(error), 'error');
    } finally {
      setBusy(false);
      setSyncActivity('');
    }
  };

  const applyResync = async () => {
    const unresolved = duplicateDecisions.some(item => item.selected && item.notesResolution === '');
    if (unresolved) {
      showStatusMessage('Choose which Notes to keep for every selected Notes conflict.', 'error');
      return;
    }
    if (!confirmingResync) {
      setConfirmingResync(true);
      return;
    }
    setBusy(true);
    setSyncActivity('resync-apply');
    try {
      const decisions = {
        restoreItemIds: restoreIds,
        duplicateResolutions: duplicateDecisions
          .filter(item => item.selected)
          .map(({ selected: _selected, ...item }) => item),
      };
      const result = await invoke('apply_complete_resync', { decisions });
      setResync(null);
      setConfirmingResync(false);
      showStatusMessage(result.followUpError
        ? `The resync plan was applied, but the full upload needs another attempt: ${result.followUpError}`
        : `Complete resync finished. ${result.restored} restored, ${result.merged} duplicates merged.`,
      result.followUpError ? 'error' : 'success');
    } catch (error) {
      showStatusMessage(String(error), 'error');
    } finally {
      setBusy(false);
      setSyncActivity('');
      refresh();
    }
  };

  const activeSyncActivity = syncActivity || (status.syncing ? 'sync' : '');
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
  const engineError = status.lastError && status.lastError !== acknowledgedLastError
    ? status.lastError
    : '';
  const visibleStatusMessage = activeSyncActivity
    ? syncActivityLabels[activeSyncActivity]
    : message || engineError;
  const visibleStatusTone = statusTone(activeSyncActivity, message, messageTone, engineError);
  const differingActivationItems = (notesActivation?.items || []).filter(item => item.differs);
  const scrollToNotesConfirmation = () => {
    const target = notesConfirmationRef.current;
    if (!target) return;
    const reducedMotion = globalThis.matchMedia?.('(prefers-reduced-motion: reduce)')?.matches;
    target.scrollIntoView({ behavior: reducedMotion ? 'auto' : 'smooth', block: 'center' });
    target.querySelector('button.primary')?.focus({ preventScroll: true });
  };

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
      {!status.initialSyncConfigured ? (
        <section className="card first-sync">
          <p className="eyebrow">BEFORE THE FIRST SYNC</p>
          <h2>How should existing shots be handled?</h2>
          <label className="toggle">
            <input type="checkbox" checked={reuseMatching} onChange={event => {
              setReuseMatching(event.currentTarget.checked);
              setPolicyDirty(true);
            }} />
            <span>Reuse matching shots already in MyBrewFolio</span>
          </label>
          <p className="muted">{REUSE_MATCHING_SHOTS_EXPLANATION}</p>
          <button type="button" className="primary compact-button" disabled={busy} onClick={configure}>
            <ActionLabel active={syncActivity === 'first-sync'} activeText="Starting first sync…">Save and start first sync</ActionLabel>
          </button>
        </section>
      ) : null}
      {(status.conflicts || status.suppressed) ? (
        <section className="card attention">
          <strong>{status.conflicts + status.suppressed} items not synchronized</strong>
          <p>Open the affected brew in MyBrewFolio to review Shot or Notes conflicts. Other items are listed under Account → MyBrewFolio Sync → Not synchronized.</p>
        </section>
      ) : null}
      {status.issues?.length ? (
        <details className="card issue-details" open>
          <summary><strong>{status.issues.length} items not synchronized</strong></summary>
          <ul>{status.issues.map(issue => (
            <li key={`${issue.kind}:${issue.sourceKey}:${issue.stage}`}>
              <strong>{issue.kind} {issue.sourceKey}</strong>
              <span>
                {issue.reason} · {issue.attempts} attempt{issue.attempts === 1 ? '' : 's'} ·{' '}
                {formatDate(issue.updatedAt * 1000)}
              </span>
            </li>
          ))}</ul>
          <button type="button" className="secondary inline-action" disabled={busy} onClick={retryFailures}>
            <ActionLabel active={syncActivity === 'retry'} activeText="Retrying…">Retry failed items</ActionLabel>
          </button>
        </details>
      ) : null}
      <h2 className="section-title">Settings</h2>
      <section className="card settings">
        <h3>GaggiMate settings</h3>
        <div className="inline-field"><input aria-label="GaggiMate hostname or local IP" value={host} onInput={event => setHost(event.currentTarget.value)} /><button type="button" onClick={saveHost} disabled={busy}>Save</button></div>
        <div className="setting-with-info">
          <label className="toggle">
            <input type="checkbox" checked={reuseMatching} onChange={event => {
              setReuseMatching(event.currentTarget.checked);
              setPolicyDirty(true);
            }} />
            <span>Reuse matching MyBrewFolio shots</span>
          </label>
          <button
            type="button"
            className="info-button"
            aria-label="Explain reuse matching MyBrewFolio shots"
            aria-expanded={showMatchingInfo}
            aria-controls="reuse-matching-shots-info"
            onClick={() => setShowMatchingInfo(current => !current)}
          >
            <InfoIcon />
          </button>
        </div>
        {showMatchingInfo ? (
          <p className="setting-info muted" id="reuse-matching-shots-info">
            {REUSE_MATCHING_SHOTS_EXPLANATION}
          </p>
        ) : null}
        {status.initialSyncConfigured && policyDirty ? (
          <button type="button" className="secondary inline-action" disabled={busy} onClick={configure}>Save matching preference</button>
        ) : null}
        <p className="muted">Shots and profiles sync one way to MyBrewFolio. Two-way Notes Sync is optional.</p>
        <div className="action-with-info">
          <button type="button" className="secondary inline-action" disabled={busy} onClick={previewResync}>
            <ActionLabel active={syncActivity === 'resync-preview'} activeText="Reading library…">Complete resync</ActionLabel>
          </button>
          <button
            type="button"
            className="info-button"
            aria-label="Explain complete resync"
            aria-expanded={showCompleteResyncInfo}
            aria-controls="complete-resync-info"
            onClick={() => setShowCompleteResyncInfo(current => !current)}
          >
            <InfoIcon />
          </button>
        </div>
        {showCompleteResyncInfo ? (
          <p className="setting-info action-info muted" id="complete-resync-info">
            {COMPLETE_RESYNC_EXPLANATION}
          </p>
        ) : null}
      </section>
      <section className="card settings notes-sync-settings">
        <div className="settings-title-with-info">
          <h3>Notes sync and backups</h3>
          <button
            type="button"
            className="info-button info-button-inline"
            aria-label="Explain two-way Notes Sync"
            aria-expanded={showNotesSyncInfo}
            aria-controls="two-way-notes-info"
            onClick={() => setShowNotesSyncInfo(current => !current)}
          >
            <InfoIcon />
          </button>
        </div>
        {showNotesSyncInfo ? (
          <p className="setting-info action-info muted" id="two-way-notes-info">
            Two-way Notes Sync writes only Notes for matching GaggiMate shots. Before activation, Sync backs up every available machine Note. When copies differ, MyBrewFolio is preselected and you can review every choice before anything is written.
          </p>
        ) : null}
        {status.notesSyncStatus === 'two_way' ? (
          <>
            <p><strong>Two-way Notes Sync is active on this computer.</strong></p>
            <div className="button-row">
              <button type="button" className="secondary inline-action" disabled={busy} onClick={createNotesBackup}>
                <ActionLabel active={syncActivity === 'notes-backup'} activeText="Creating backup…">Create Latest Backup</ActionLabel>
              </button>
              <button type="button" className="secondary inline-action danger-action" disabled={busy} onClick={disableNotesSync}>Turn off two-way Notes Sync</button>
            </div>
          </>
        ) : status.notesSyncStatus === 'activation_pending' ? (
          <>
            <p><strong>Two-way Notes Sync is waiting for activation.</strong></p>
            {status.notesSyncTargetDeviceId === status.thisDeviceId ? (
              <button type="button" className="primary compact-button" disabled={busy} onClick={beginNotesActivation}>
                <ActionLabel active={syncActivity === 'notes-activation'} activeText="Creating backup…">Create backup and review Notes</ActionLabel>
              </button>
            ) : <p className="muted">The selected Sync computer must finish the backup and review.</p>}
            <button type="button" className="secondary inline-action" disabled={busy} onClick={disableNotesSync}>Cancel activation</button>
          </>
        ) : (
          <>
            <p><strong>Two-way Notes Sync is off.</strong></p>
            <button type="button" className="primary compact-button" disabled={busy} onClick={beginNotesActivation}>
              <ActionLabel active={syncActivity === 'notes-activation'} activeText="Creating backup…">Set up two-way Notes Sync</ActionLabel>
            </button>
          </>
        )}
        {(status.noteBackups || []).length ? (
          <div className="backup-list">
            {status.noteBackups.map(backup => (
              <article className="backup-row" key={backup.id}>
                <div><strong>{backup.slot === 'activation' ? 'First Backup' : 'Latest Backup'}</strong><small>{backup.itemCount} shots · {formatDate(backup.finalizedAt || backup.createdAt)}</small></div>
                <button type="button" className="secondary compact-button" disabled={busy} onClick={() => previewRestore(backup)}>Restore</button>
              </article>
            ))}
          </div>
        ) : null}
      </section>
      <h2 className="section-title">App settings</h2>
      <section className="card settings">
        <h3>Updates</h3>
        {availableUpdate ? (
          <output className="update-available" aria-live="polite">
            Update {availableUpdate} is available.
          </output>
        ) : null}
        <button type="button" className="secondary inline-action" disabled={busy} onClick={update}>
          {availableUpdate ? `Install update ${availableUpdate}` : 'Check for updates'}
        </button>
        <p className="muted app-version">Installed version {appVersion || '…'}</p>
      </section>
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
      {notesIntroOpen ? (
        <section className="card modal-card" role="dialog" aria-labelledby="notes-intro-title">
          <p className="eyebrow">OPTIONAL</p>
          <h2 id="notes-intro-title">Keep your shot Notes in sync both ways?</h2>
          <div className="intro-with-info">
            <p>MyBrewFolio can update Notes on matching GaggiMate shots. Shots and profiles stay one-way.</p>
            <button
              type="button"
              className="info-button info-button-inline"
              aria-label="Explain two-way Notes Sync activation"
              aria-expanded={showNotesIntroInfo}
              aria-controls="two-way-notes-intro-info"
              onClick={() => setShowNotesIntroInfo(current => !current)}
            >
              <InfoIcon />
            </button>
          </div>
          {showNotesIntroInfo ? (
            <p className="muted" id="two-way-notes-intro-info">Sync first creates a complete GaggiMate Notes backup. If copies differ, MyBrewFolio is preselected and every choice can be reviewed before a Note is written.</p>
          ) : null}
          <div className="dialog-actions">
            <button type="button" className="secondary compact-button" disabled={busy} onClick={dismissNotesIntro}>Not now</button>
            <button type="button" className="primary compact-button" disabled={busy} onClick={beginNotesActivation}>
              <ActionLabel active={syncActivity === 'notes-activation'} activeText="Creating backup…">Create backup and review</ActionLabel>
            </button>
          </div>
        </section>
      ) : null}
      {notesActivation ? (
        <section className="card modal-card" role="dialog" aria-labelledby="notes-activation-title">
          <div className="modal-title-row">
            <h2 id="notes-activation-title">Review Notes to sync</h2>
            {differingActivationItems.length >= 6 ? (
              <button type="button" className="scroll-confirm-button" aria-label="Go to confirmation" onClick={scrollToNotesConfirmation}>
                <DownArrowIcon />
              </button>
            ) : null}
          </div>
          <p>Notes backup complete. Select which Notes to keep: the MyBrewFolio version or the version on your machine.</p>
          {differingActivationItems.length ? (
            <fieldset>
              <legend>Different Notes</legend>
              <div className="bulk-actions">
                <button type="button" className="secondary inline-action" onClick={() => setNotesDecisions(Object.fromEntries(differingActivationItems.map(item => [item.sourceKey, 'mybrewfolio'])))}>Use MyBrewFolio for all</button>
                <button type="button" className="secondary inline-action" onClick={() => setNotesDecisions(Object.fromEntries(differingActivationItems.map(item => [item.sourceKey, 'gaggimate'])))}>Use GaggiMate for all</button>
              </div>
              {differingActivationItems.map(item => (
                <label className="activation-choice" key={item.sourceKey}>
                  <span><strong>{item.displayName}</strong><small>Shot {item.sourceKey}</small></span>
                  <select aria-label={`Notes source for ${item.displayName}`} value={notesDecisions[item.sourceKey] || 'mybrewfolio'} onChange={event => setNotesDecisions(current => ({ ...current, [item.sourceKey]: event.currentTarget.value }))}>
                    <option value="mybrewfolio">Use MyBrewFolio Notes</option>
                    <option value="gaggimate">Use GaggiMate Notes</option>
                  </select>
                </label>
              ))}
            </fieldset>
          ) : <p className="muted">All matching Notes already agree. No initial overwrite is needed.</p>}
          <div className="dialog-actions" ref={notesConfirmationRef}>
            <button type="button" className="secondary compact-button" disabled={busy} onClick={() => setNotesActivation(null)}>Cancel</button>
            <button type="button" className="primary compact-button" disabled={busy} onClick={confirmNotesActivation}>
              <ActionLabel active={syncActivity === 'notes-write'} activeText="Enabling…">Enable two-way Notes Sync</ActionLabel>
            </button>
          </div>
        </section>
      ) : null}
      {restorePreview ? (
        <section className="card modal-card" role="dialog" aria-labelledby="restore-notes-title">
          <h2 id="restore-notes-title">Restore GaggiMate Notes</h2>
          <p>Select Notes to restore. Only shots whose machine ID and recording time still match can be written.</p>
          <fieldset className="restore-list">
            {(restorePreview.items || []).map(item => {
              const key = item.source_key || item.sourceKey;
              return <label className="toggle" key={key}>
                <input type="checkbox" disabled={!item.available} checked={restoreKeys.includes(key)} onChange={event => setRestoreKeys(current => event.currentTarget.checked ? [...current, key] : current.filter(value => value !== key))} />
                <span>{key}{item.available ? '' : ' · no longer available on this GaggiMate'}</span>
              </label>;
            })}
          </fieldset>
          <div className="dialog-actions">
            <button type="button" className="secondary compact-button" disabled={busy} onClick={() => setRestorePreview(null)}>Cancel</button>
            <button type="button" className="primary compact-button" disabled={busy || !restoreKeys.length} onClick={restoreNotes}>Restore selected Notes</button>
          </div>
        </section>
      ) : null}
      {resync ? (
        <ResyncPreview
          resync={resync}
          restoreIds={restoreIds}
          setRestoreIds={setRestoreIds}
          duplicateDecisions={duplicateDecisions}
          setDuplicateDecisions={setDuplicateDecisions}
          confirmingResync={confirmingResync}
          setConfirmingResync={setConfirmingResync}
          busy={busy}
          syncActivity={syncActivity}
          applyResync={applyResync}
          onCancel={() => setResync(null)}
        />
      ) : null}
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
