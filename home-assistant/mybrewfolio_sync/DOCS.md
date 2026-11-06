# Setup

In Home Assistant, open **Settings → Apps → Install app → Repositories** and add `https://github.com/modsmthng/MyBrewFolio-Sync#home-assistant`. Then find **MyBrewFolio Sync** in the app store.

1. Install and start **MyBrewFolio Sync**, then select **Open Web UI**. The UI is available to Home Assistant administrators; no extra port or password is needed.
2. Select **Connect MyBrewFolio** and approve the connection in your browser. Return to Home Assistant. An unused link renews automatically after ten minutes. Your MyBrewFolio sign-in is separate from Home Assistant.
3. Choose an existing account machine or create a new one, then enter its local address. If `gaggimate.local` does not resolve, use a fixed private LAN IP. Repeat for additional machines.
4. Open **Sync settings in MyBrewFolio**, select the machine, choose its import preference and start the first sync. Manage schedules, Notes, backups, conflicts and resync there too.

You can connect the same machine from another Sync installation, but each connection must be managed separately. Only one installation can write two-way Notes for a machine.

## Connections and troubleshooting

The app shows machine addresses, status, counts and errors. You can change a name or address, remove a machine from this installation, or disconnect the account. Removing a connection does not delete your MyBrewFolio history or machine content. Disconnecting the account affects all machines on this installation.

If a machine is offline, Sync resumes when it becomes reachable. For persistent errors, check its LAN IP, VLAN routing and internet access. If the installation was revoked, reconnect the account. After a restart or update, reload the web UI if it reports that the session changed.

## Updates and backups

Use Home Assistant's app update controls. Its backups stop Sync and include the private app data, including the encryption key. Restore the complete app backup and protect it like account credentials. Do not run the old and restored copies at the same time: they represent the same installation.

## Home Assistant integration

This app imports your library; it does not provide Home Assistant entities or automation actions. The separate [GaggiMate integration](https://github.com/gaggimate/ha-integration) provides sensors and controls and is not required by Sync. HACS is not required either.

The app requires Home Assistant OS. Home Assistant Container users can use the [Docker companion](https://github.com/modsmthng/MyBrewFolio-Sync/blob/main/docs/headless.md).

[Support](https://mybrewfolio.com/support/sync) · [Privacy](https://mybrewfolio.com/legal/storage#sync-companion)
