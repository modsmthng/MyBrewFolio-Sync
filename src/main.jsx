// SPDX-License-Identifier: GPL-3.0-or-later

import { render } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { invoke } from '@tauri-apps/api/core';
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
};

function formatDate(value) {
  if (!value) return 'Not synced yet';
  const date = new Date(value);
  return Number.isFinite(date.getTime()) ? date.toLocaleString() : 'Not synced yet';
}

function StatusPill({ status }) {
  const kind = status.syncing ? 'working' : status.lastError ? 'error' : status.connected ? 'ok' : 'idle';
  const text = status.syncing ? 'Syncing' : status.lastError ? 'Needs attention' : status.connected ? 'Connected' : 'Not connected';
  return <span className={`status status-${kind}`}>{text}</span>;
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

  useEffect(() => { isEnabled().then(setAutostart).catch(() => setAutostart(false)); }, []);

  const syncNow = async () => {
    setBusy(true);
    setMessage('');
    try {
      await invoke('sync_now');
      setMessage('Synchronization completed.');
    } catch (error) {
      setMessage(String(error));
    } finally {
      setBusy(false);
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

  return (
    <main className="shell">
      <header className="brand-row dashboard-header">
        <div><div className="mark compact">my<br />brew<br />folio</div><h1>Sync</h1></div>
        <StatusPill status={status} />
      </header>
      <section className="overview card">
        <div><small>Last successful sync</small><strong>{formatDate(status.lastSyncAt)}</strong></div>
        <button className="primary compact-button" disabled={busy || status.syncing} onClick={syncNow}>{busy || status.syncing ? 'Syncing…' : 'Sync now'}</button>
      </section>
      {status.lastError ? <section className="alert"><strong>Sync needs attention</strong><p>{status.lastError}</p></section> : null}
      <section className="counts">
        <article className="card"><strong>{status.shots}</strong><span>Shots</span></article>
        <article className="card"><strong>{status.profiles}</strong><span>Profiles</span></article>
        <article className="card"><strong>{status.notes}</strong><span>Notes</span></article>
      </section>
      {(status.conflicts || status.suppressed) ? (
        <section className="card attention"><strong>{status.conflicts} conflicts · {status.suppressed} suppressed</strong><p>Open Account → MyBrewFolio Sync on the website to review these items.</p></section>
      ) : null}
      <section className="card settings">
        <h2>GaggiMate</h2>
        <div className="inline-field"><input value={host} onInput={event => setHost(event.currentTarget.value)} /><button onClick={saveHost} disabled={busy}>Save</button></div>
        <label className="toggle"><input type="checkbox" checked={autostart} onChange={toggleAutostart} /><span>Start Sync with this computer</span></label>
        <p className="muted">Sync is one-way. Nothing is selected, overwritten, or deleted on your GaggiMate.</p>
        <button className="secondary inline-action" disabled={busy} onClick={update}>Check for updates</button>
      </section>
      <button className="secondary" disabled={busy} onClick={disconnect}>Disconnect account</button>
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
