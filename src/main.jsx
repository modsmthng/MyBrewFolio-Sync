// SPDX-License-Identifier: GPL-3.0-or-later

import { render } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { invoke } from '@tauri-apps/api/core';
import { getVersion } from '@tauri-apps/api/app';
import { listen } from '@tauri-apps/api/event';
import { getCurrent, onOpenUrl } from '@tauri-apps/plugin-deep-link';
import { enable, disable, isEnabled } from '@tauri-apps/plugin-autostart';
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
  issues: [],
};

function formatDate(value) {
  if (!value) return 'Not synced yet';
  const date = new Date(value);
  return Number.isFinite(date.getTime()) ? date.toLocaleString() : 'Not synced yet';
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

function StatusPill({ status }) {
  const kind = status.syncing ? 'working' : status.lastError ? 'error' : status.connected ? 'ok' : 'idle';
  const text = status.syncing ? 'Syncing' : status.lastError ? 'Needs attention' : status.connected ? 'Connected' : 'Not connected';
  return <span className={`status status-${kind}`}>{status.syncing ? <SyncSpinner /> : null}{text}</span>;
}

function Setup({ status, refresh, externalMessage }) {
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
        <span className="experimental">EXPERIMENTAL</span>
      </header>
      <section className="hero">
        <p className="eyebrow">MYBREWFOLIO SYNC</p>
        <h1>Your GaggiMate library, available everywhere.</h1>
        <p>Shots, profiles, and notes are copied to your private MyBrewFolio library. Nothing is changed on your machine.</p>
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
      <button className="primary" disabled={busy} onClick={connect}>{busy ? 'Opening browser…' : 'Connect MyBrewFolio'}</button>
      {message || externalMessage ? <p className="message" aria-live="polite">{message || externalMessage}</p> : null}
      <p className="privacy">The local address stays on this computer. Only the library content you synchronize is sent to MyBrewFolio.</p>
    </main>
  );
}

function Dashboard({ status, refresh }) {
  const [autostart, setAutostart] = useState(true);
  const [host, setHost] = useState(status.machineHost);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState('');
  const [reuseMatching, setReuseMatching] = useState(status.duplicatePolicy !== 'import_all');
  const [policyDirty, setPolicyDirty] = useState(false);
  const [appVersion, setAppVersion] = useState('');
  const [resync, setResync] = useState(null);
  const [restoreIds, setRestoreIds] = useState([]);
  const [duplicateDecisions, setDuplicateDecisions] = useState([]);
  const [syncActivity, setSyncActivity] = useState('');

  useEffect(() => {
    isEnabled().then(setAutostart).catch(() => setAutostart(false));
    getVersion().then(setAppVersion).catch(() => setAppVersion('Unknown'));
  }, []);
  useEffect(() => {
    if (!policyDirty) setReuseMatching(status.duplicatePolicy !== 'import_all');
  }, [status.duplicatePolicy, policyDirty]);

  const syncNow = async () => {
    setBusy(true);
    setSyncActivity('sync');
    setMessage('');
    try {
      await invoke('sync_now');
      setMessage('Synchronization completed.');
    } catch (error) {
      setMessage(String(error));
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
      setMessage('Machine address saved.');
      refresh();
    } catch (error) {
      setMessage(String(error));
    } finally {
      setBusy(false);
    }
  };

  const toggleAutostart = async event => {
    const checked = event.currentTarget.checked;
    setAutostart(checked);
    try {
      if (checked) await enable(); else await disable();
    } catch (error) {
      setAutostart(!checked);
      setMessage(String(error));
    }
  };

  const disconnect = async () => {
    if (!confirm('Disconnect this Sync installation from MyBrewFolio?')) return;
    setBusy(true);
    try {
      await invoke('disconnect_account');
      refresh();
    } catch (error) {
      setMessage(String(error));
    } finally {
      setBusy(false);
    }
  };

  const update = async () => {
    setBusy(true);
    setMessage('Checking for updates…');
    try {
      const result = await invoke('install_update');
      const messages = {
        installed: 'The update was installed. Restart Sync to use the new version.',
        'up-to-date': 'MyBrewFolio Sync is up to date.',
        'store-managed': 'Updates are managed by Microsoft Store.',
        'not-configured': 'Automatic updates are not available in this development build.',
      };
      setMessage(messages[result] || 'Update check completed.');
    } catch (error) {
      setMessage(String(error));
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
      setMessage(status.initialSyncConfigured
        ? 'Matching preference saved.'
        : 'Sync preferences saved and the first synchronization completed.');
    } catch (error) {
      setMessage(String(error));
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
      setMessage('Failed items were checked again.');
    } catch (error) {
      setMessage(String(error));
    } finally {
      setBusy(false);
      setSyncActivity('');
      refresh();
    }
  };

  const previewResync = async () => {
    setBusy(true);
    setSyncActivity('resync-preview');
    setMessage('Reading the complete GaggiMate library…');
    try {
      const preview = await invoke('preview_complete_resync');
      setResync(preview);
      setRestoreIds((preview.restoreItems || []).map(item => item.id));
      setDuplicateDecisions((preview.duplicates || []).map(item => ({
        mappingId: item.mapping_id,
        keepShotId: item.keep_shot_id,
        removeShotId: item.remove_shot_id,
        selected: true,
        notesResolution: item.notes_conflict ? '' : undefined,
      })));
      setMessage('');
    } catch (error) {
      setMessage(String(error));
    } finally {
      setBusy(false);
      setSyncActivity('');
    }
  };

  const applyResync = async () => {
    const unresolved = duplicateDecisions.some(item => item.selected && item.notesResolution === '');
    if (unresolved) {
      setMessage('Choose which notes to keep for every selected notes conflict.');
      return;
    }
    if (!confirm('Apply this complete resync? Selected deleted items will be restored and selected duplicate Sync copies will be removed.')) return;
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
      setMessage(result.followUpError
        ? `The resync plan was applied, but the full upload needs another attempt: ${result.followUpError}`
        : `Complete resync finished. ${result.restored} restored, ${result.merged} duplicates merged.`);
    } catch (error) {
      setMessage(String(error));
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
  };

  return (
    <main className="shell">
      <header className="brand-row dashboard-header">
        <div><div className="mark compact">my<br />brew<br />folio</div><h1>Sync</h1></div>
        <StatusPill status={status} />
      </header>
      <section className="overview card">
        <div><small>Last successful sync</small><strong>{formatDate(status.lastSyncAt)}</strong></div>
        <button className="primary compact-button" disabled={busy || status.syncing} onClick={syncNow}>
          <ActionLabel active={activeSyncActivity === 'sync'} activeText="Syncing…">Sync now</ActionLabel>
        </button>
      </section>
      {activeSyncActivity ? (
        <section className="sync-activity" role="status" aria-live="polite">
          <SyncSpinner />
          <strong>{syncActivityLabels[activeSyncActivity]}</strong>
        </section>
      ) : null}
      {status.lastError ? <section className="alert"><strong>Sync needs attention</strong><p>{status.lastError}</p></section> : null}
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
          <p className="muted">Matches use the GaggiMate shot ID and recording time. Existing MyBrewFolio changes are not silently overwritten.</p>
          <button className="primary compact-button" disabled={busy} onClick={configure}>
            <ActionLabel active={syncActivity === 'first-sync'} activeText="Starting first sync…">Save and start first sync</ActionLabel>
          </button>
        </section>
      ) : null}
      {(status.conflicts || status.suppressed) ? (
        <section className="card attention">
          <strong>{status.conflicts} conflicts · {status.suppressed} suppressed</strong>
          <p>Go to MyBrewFolio.com, then open Account → MyBrewFolio Sync to review these items or allow all suppressed imports at once.</p>
        </section>
      ) : null}
      {status.issues?.length ? (
        <details className="card issue-details" open>
          <summary><strong>{status.issues.length} items need attention</strong></summary>
          <ul>{status.issues.map(issue => (
            <li key={`${issue.kind}:${issue.sourceKey}:${issue.stage}`}>
              <strong>{issue.kind} {issue.sourceKey}</strong>
              <span>
                {issue.reason} · {issue.attempts} attempt{issue.attempts === 1 ? '' : 's'} ·{' '}
                {formatDate(issue.updatedAt * 1000)}
              </span>
            </li>
          ))}</ul>
          <button className="secondary inline-action" disabled={busy} onClick={retryFailures}>
            <ActionLabel active={syncActivity === 'retry'} activeText="Retrying…">Retry failed items</ActionLabel>
          </button>
        </details>
      ) : null}
      <h2 className="section-title">Settings</h2>
      <section className="card settings">
        <h3>GaggiMate settings</h3>
        <div className="inline-field"><input value={host} onInput={event => setHost(event.currentTarget.value)} /><button onClick={saveHost} disabled={busy}>Save</button></div>
        <label className="toggle">
          <input type="checkbox" checked={reuseMatching} onChange={event => {
            setReuseMatching(event.currentTarget.checked);
            setPolicyDirty(true);
          }} />
          <span>Reuse matching MyBrewFolio shots</span>
        </label>
        {status.initialSyncConfigured && policyDirty ? (
          <button className="secondary inline-action" disabled={busy} onClick={configure}>Save matching preference</button>
        ) : null}
        <p className="muted">Sync is one-way. Nothing is selected, overwritten, or deleted on your GaggiMate.</p>
        <button className="secondary inline-action" disabled={busy} onClick={previewResync}>
          <ActionLabel active={syncActivity === 'resync-preview'} activeText="Reading library…">Complete resync</ActionLabel>
        </button>
      </section>
      <section className="card settings">
        <h3>App settings</h3>
        <label className="toggle"><input type="checkbox" checked={autostart} onChange={toggleAutostart} /><span>Start Sync with this computer</span></label>
        <button className="secondary inline-action" disabled={busy} onClick={update}>Check for updates</button>
        <p className="muted app-version">Installed version {appVersion || '…'}</p>
      </section>
      {resync ? (
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
              <button className="secondary inline-action" onClick={() => setDuplicateDecisions(current => current.map(item => ({ ...item, selected: true })))}>Select all</button>
              <button className="secondary inline-action" onClick={() => setDuplicateDecisions(current => current.map(item => item.notesResolution === '' ? { ...item, notesResolution: 'mybrewfolio' } : item))}>Keep MyBrewFolio notes for all</button>
              <button className="secondary inline-action" onClick={() => setDuplicateDecisions(current => current.map(item => item.notesResolution === '' ? { ...item, notesResolution: 'gaggimate' } : item))}>Use GaggiMate notes for all</button>
            </div>
            {resync.duplicates.map((item, index) => <div className="duplicate-row" key={item.mapping_id}>
              <label className="toggle">
                <input type="checkbox" checked={duplicateDecisions[index]?.selected} onChange={event => setDuplicateDecisions(current => current.map((decision, position) => position === index ? { ...decision, selected: event.currentTarget.checked } : decision))} />
                <span>Keep “{item.keep_name}” and remove Sync copy “{item.remove_name}”</span>
              </label>
              {item.notes_conflict ? <select value={duplicateDecisions[index]?.notesResolution || ''} onChange={event => setDuplicateDecisions(current => current.map((decision, position) => position === index ? { ...decision, notesResolution: event.currentTarget.value } : decision))}>
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
            <button className="secondary inline-action" disabled={busy} onClick={() => setResync(null)}>Cancel</button>
            <button className="primary compact-button" disabled={busy} onClick={applyResync}>
              <ActionLabel active={syncActivity === 'resync-apply'} activeText="Applying resync…">Apply complete resync</ActionLabel>
            </button>
          </div>
        </section>
      ) : null}
      <section className="card account-action">
        <h3>Account</h3>
        <p className="muted">This installation is connected to your private MyBrewFolio library.</p>
        <button className="secondary" disabled={busy} onClick={disconnect}>Disconnect account</button>
      </section>
      {message ? <p className="message" aria-live="polite">{message}</p> : null}
    </main>
  );
}

function App() {
  const [status, setStatus] = useState(initialStatus);
  const [loading, setLoading] = useState(true);
  const [oauthError, setOauthError] = useState('');
  const refresh = async () => {
    try { setStatus(await invoke('get_status')); } finally { setLoading(false); }
  };

  useEffect(() => {
    refresh();
    const poll = setInterval(refresh, 5000);
    let unlistenDeepLink;
    let unlistenStatus;
    let unlistenSync;
    const handleUrls = urls => {
      const callback = urls?.find(url => url.startsWith('mybrewfolio-sync://oauth/callback'));
      if (callback) {
        setOauthError('');
        invoke('complete_oauth', { callbackUrl: callback })
          .then(refresh)
          .catch(error => setOauthError(`MyBrewFolio could not finish connecting this installation: ${String(error)}`));
      }
    };
    getCurrent().then(handleUrls).catch(() => {});
    onOpenUrl(handleUrls).then(unlisten => { unlistenDeepLink = unlisten; });
    listen('sync-status-changed', refresh).then(unlisten => { unlistenStatus = unlisten; });
    listen('sync-requested', () => invoke('sync_now').finally(refresh)).then(unlisten => { unlistenSync = unlisten; });
    return () => {
      clearInterval(poll);
      unlistenDeepLink?.();
      unlistenStatus?.();
      unlistenSync?.();
    };
  }, []);

  if (loading) return <main className="shell loading">Loading MyBrewFolio Sync…</main>;
  return status.connected
    ? <Dashboard status={status} refresh={refresh} />
    : <Setup status={status} refresh={refresh} externalMessage={oauthError} />;
}

render(<App />, document.getElementById('app'));
