# Setup

1. Install and start **MyBrewFolio Sync**, then select **Open Web UI**. The UI is available to
   Home Assistant administrators through Ingress; no additional port or password is needed.
2. Select **Connect MyBrewFolio**. Approve the connection in the new browser tab. Return to
   Home Assistant; the page updates automatically. An unused link expires after ten minutes and
   renews automatically. MyBrewFolio sign-in is separate from your Home Assistant login.
3. Choose an existing account machine or create a new named one, and enter its local address.
   Prefer a fixed private LAN IP if `gaggimate.local` does not resolve from the app. Repeat this
   step for additional machines. Existing machines are never chosen implicitly.
4. Open **Sync settings in MyBrewFolio**, select the machine and choose its import preference
   before starting the first sync. Matching, schedules, Notes, backups, restore, conflicts and
   resync are managed there.

Do not run another Sync installation against the same machine unless you intend to maintain both
connections. The existing account capability and writer rules still apply.

## Connections

The UI shows each machine's address, reachability, last sync, item counts and current errors.
Change a name or local address with its Save button. Removing a connection affects only this
installation; MyBrewFolio history and machine content remain. Disconnecting the account is a
separate confirmed action affecting all machines in this installation.

A failed registration is retried automatically and does not expose machine setup as ready.
If the installation was revoked, reconnect the account. If a machine is offline, synchronization
resumes automatically when it is reachable again. Check the fixed LAN IP, VLAN routing and
outgoing internet access if connection errors persist.

## Updates and backups

Use Home Assistant's app update controls. No desktop updater or separate update service runs.
The entire private state is under `/data/state`: the SQLite databases, queues and encrypted
credentials together with their original encryption key. Keep this data across updates.

Home Assistant backups include this private app data and stop Sync while copying it (`backup: cold`).
Restore the complete app backup, including its key. Treat the backup as private account data.
Do not start the old and restored copies simultaneously: they represent the same installation.
After an update/restart, reload the web UI if an old tab reports that its session changed.

The app runs as UID/GID 10001. Its startup wrapper initializes only `/data/state` and leaves
Supervisor's `/data/options.json` untouched. No host networking, privileged mode, Docker socket,
Home Assistant Core API access or Supervisor API access is required.

## GaggiMate integration and installation types

This app imports your library; it does not provide Home Assistant entities or automation actions.
The independent [GaggiMate integration](https://github.com/gaggimate/ha-integration) can continue
to provide machine sensors and controls. This app has no dependency on it or HACS.

Home Assistant OS is required for app installation. Home Assistant Container users can run the
[normal Docker companion](https://github.com/modsmthng/MyBrewFolio-Sync/blob/main/docs/headless.md).

[Support](https://mybrewfolio.com/support/sync) · [Privacy](https://mybrewfolio.com/legal/storage#sync-companion)
