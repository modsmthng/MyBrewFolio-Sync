// SPDX-License-Identifier: GPL-3.0-or-later
import { useEffect, useRef, useState } from 'preact/hooks';
import { request, pairingLink } from './api.js';
import logo from '../../assets/textlogosync.svg';

const ACCOUNT = 'https://mybrewfolio.com/account/sync';
const external = { target: '_blank', rel: 'noopener noreferrer' };

function Machine({ machine, act, busy }) {
  const [name, setName] = useState(machine.name);
  const [host, setHost] = useState(machine.machineHost);
  const [remove, setRemove] = useState(false);
  const previous = useRef({ name: machine.name, host: machine.machineHost });
  useEffect(() => {
    if (previous.current.name !== machine.name) setName(machine.name);
    if (previous.current.host !== machine.machineHost) setHost(machine.machineHost);
    previous.current = { name: machine.name, host: machine.machineHost };
  }, [machine.name, machine.machineHost]);
  return <section className="card">
    <header className="machine-heading"><h2>{machine.name}</h2>
      <span className="badge">{machine.syncing ? 'Synchronizing…' : machine.machineReachable ? 'Reachable' : 'Not reachable'}</span>
    </header>
    <p className="muted">{machine.lastSyncAt ? `Last sync: ${new Date(machine.lastSyncAt).toLocaleString()}` : 'Not synced yet'}</p>
    {machine.lastError && <p role="status" className="error">{machine.lastError}</p>}
    {!machine.initialSyncConfigured && <p>Choose your import preference in <a href={ACCOUNT} {...external}>MyBrewFolio</a> to start the first sync.</p>}
    <form onSubmit={event => { event.preventDefault(); act({ action: 'renameMachine', machineId: machine.machineId, name }); }}>
      <label>Machine name<input value={name} maxLength={24} required onInput={event => setName(event.target.value)} /></label>
      <button disabled={busy || name === machine.name}>Save name</button>
    </form>
    <form onSubmit={event => { event.preventDefault(); act({ action: 'setHost', machineId: machine.machineId, host }); }}>
      <label>Local address<input value={host} required autoCapitalize="off" spellCheck={false} onInput={event => setHost(event.target.value)} /></label>
      <button disabled={busy || host === machine.machineHost}>Save address</button>
    </form>
    <p className="muted">{machine.shots} brews · {machine.profiles} profiles · {machine.notes} notes</p>
    <div className="actions"><a href={ACCOUNT} {...external}>Sync settings in MyBrewFolio ↗</a>
      <button className="text-button" disabled={busy} onClick={() => setRemove(true)}>Remove connection</button></div>
    {remove && <div className="confirmation" role="group" aria-label={`Remove ${machine.name}`}>
      <p>Disconnect this installation from {machine.name}? Your MyBrewFolio history and the machine's content remain available.</p>
      <div className="actions"><button disabled={busy} onClick={async () => {
        if (await act({ action: 'removeMachine', machineId: machine.machineId, confirm: true })) setRemove(false);
      }}>Confirm removal</button><button disabled={busy} onClick={() => setRemove(false)}>Cancel</button></div>
    </div>}
  </section>;
}

function AddMachine({ machines, act, busy, loadMachines }) {
  const [mode, setMode] = useState('existing');
  const [machineId, setMachineId] = useState('');
  const [name, setName] = useState('');
  const [host, setHost] = useState('');
  return <section className="card"><h2>Connect a machine</h2>
    <p>Choose an existing MyBrewFolio machine, or create a new one. Enter its private LAN IP if its .local hostname cannot be resolved.</p>
    <form onSubmit={async event => {
      event.preventDefault();
      if (await act(mode === 'existing' ? { action: 'connectMachine', machineId, host } : { action: 'addMachine', name, host })) {
        setName(''); setHost(''); setMachineId('');
      }
    }}>
      <label>Machine selection<select value={mode} onChange={event => setMode(event.target.value)}>
        <option value="existing">Existing account machine</option><option value="new">Create a new machine</option>
      </select></label>
      {mode === 'existing' ? <>
        <label>Account machine<select value={machineId} required onChange={event => setMachineId(event.target.value)}>
          <option value="">Choose a machine…</option>
          {machines.map(machine => <option key={machine.id} value={machine.id}>{machine.name}</option>)}
        </select></label>
        <button type="button" disabled={busy} onClick={loadMachines}>Refresh machine list</button>
        {!machines.length && <p className="muted">No unconnected machines listed. Refresh the list or create a new machine.</p>}
      </> : <label>New machine name<input value={name} required maxLength={24} onInput={event => setName(event.target.value)} /></label>}
      <label>Local address<input value={host} placeholder="192.168.1.42 or gaggimate.local" required autoCapitalize="off" spellCheck={false} onInput={event => setHost(event.target.value)} /></label>
      <button className="primary" disabled={busy || (mode === 'existing' && !machineId)}>Connect machine</button>
    </form>
  </section>;
}

export function App() {
  const [snapshot, setSnapshot] = useState(null);
  const [accountMachines, setAccountMachines] = useState([]);
  const [error, setError] = useState('');
  const [connectionError, setConnectionError] = useState('');
  const [notice, setNotice] = useState('');
  const [busy, setBusy] = useState(false);
  const [disconnect, setDisconnect] = useState(false);
  const refresh = async () => {
    const next = await request('status');
    setSnapshot(next); setConnectionError('');
    return next;
  };
  useEffect(() => {
    let active = true;
    let timer;
    const poll = async () => {
      try { const next = await request('status'); if (active) { setSnapshot(next); setConnectionError(''); } }
      catch (error) { if (active) setConnectionError(error.message); }
      if (active) timer = setTimeout(poll, 4000);
    };
    poll();
    return () => { active = false; clearTimeout(timer); };
  }, []);
  const loadMachines = async () => {
    try { const data = await request('machines'); setAccountMachines(data.machines); }
    catch (error) { setError(error.message); }
  };
  const setupState = snapshot?.setup?.state;
  const ready = Boolean(snapshot?.status?.connected && ['ready', 'machine_selection'].includes(setupState));
  useEffect(() => { if (ready) loadMachines(); else setAccountMachines([]); }, [ready]);
  const act = async body => {
    setBusy(true); setError(''); setNotice('');
    try {
      const result = await request('action', body, snapshot.csrfToken);
      if (result.serverRevoked === false) setNotice('Disconnected locally. Open MyBrewFolio Account → Sync to revoke the previous installation.');
      await refresh();
      if (body.action !== 'disconnect' && ready) await loadMachines();
      return true;
    } catch (error) { setError(error.message); return false; }
    finally { setBusy(false); }
  };
  const status = snapshot?.status;
  const attached = status?.machines || [];
  const ids = new Set(attached.map(machine => machine.machineId));
  const verificationUrl = pairingLink(snapshot?.setup?.verificationUrl);
  return <main className="shell">
    <header className="brand"><img src={logo} alt="MyBrewFolio Sync" /><span>Home Assistant</span></header>
    <h1>Your brews, always in sync.</h1>
    <p className="intro">Connect your GaggiMate machines to your private MyBrewFolio library.</p>
    {connectionError && <p className="error" role="alert">{connectionError}</p>}
    {error && <p className="error" role="alert">{error}</p>}
    {notice && <p role="status">{notice} <a href={ACCOUNT} {...external}>Open account ↗</a></p>}
    {!snapshot && !connectionError && <p role="status">Loading Sync…</p>}
    {snapshot && !ready && <section className="card"><h2>{status.connected ? 'Preparing your account' : 'Connect MyBrewFolio'}</h2>
      {setupState === 'error' && <p className="error" role="status">{snapshot.setup.error} Retrying automatically.</p>}
      {setupState === 'starting' && <p role="status">Starting the connection…</p>}
      {setupState === 'pairing' && <>
        <p>Sign in in a new browser tab. This page updates automatically after approval. Unused links expire after ten minutes and renew automatically.</p>
        {verificationUrl ? <a className="button primary" href={verificationUrl} {...external}>Connect MyBrewFolio ↗</a> : <p role="status">Waiting for a valid connection link…</p>}
      </>}
      {!status.connected && ['disconnected', 'ready', 'machine_selection'].includes(setupState) && <button className="primary" disabled={busy} onClick={() => act({ action: 'connect' })}>Connect MyBrewFolio</button>}
    </section>}
    {ready && <>
      <p className="connected" role="status">MyBrewFolio account connected · {attached.length} machine(s)</p>
      {attached.map(machine => <Machine key={machine.machineId} machine={machine} act={act} busy={busy} />)}
      <AddMachine machines={accountMachines.filter(machine => !ids.has(machine.id))} act={act} busy={busy} loadMachines={loadMachines} />
      <section className="card"><h2>Sync preferences</h2><p>Manage matching, schedules, Notes, backups, conflicts and recovery in MyBrewFolio.</p><a className="button primary" href={ACCOUNT} {...external}>Open MyBrewFolio Sync settings ↗</a></section>
    </>}
    {status?.connected && <section className="card"><h2>Account connection</h2>
      {disconnect ? <div className="confirmation"><p>Disconnect this account and all its machines from this installation? Your MyBrewFolio library is preserved.</p><div className="actions">
        <button disabled={busy} onClick={async () => { if (await act({ action: 'disconnect', confirm: true })) setDisconnect(false); }}>Confirm disconnect</button>
        <button disabled={busy} onClick={() => setDisconnect(false)}>Cancel</button>
      </div></div> : <button disabled={busy} onClick={() => setDisconnect(true)}>Disconnect account</button>}
    </section>}
    <footer><p>The machine addresses stay on this Home Assistant installation.</p>
      <p>Updates are managed by Home Assistant.{snapshot && ` Installed version ${snapshot.version}.`}</p>
      <a href="https://mybrewfolio.com/support/sync" {...external}>Support ↗</a> · <a href="https://mybrewfolio.com/legal/storage#sync-companion" {...external}>Privacy ↗</a>
    </footer>
  </main>;
}
