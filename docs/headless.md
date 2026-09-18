# MyBrewFolio Sync in Docker

The container runs the same synchronization engine as the desktop app. It creates its own
credentials key, connects through a browser link, and keeps state in `/data`. Manage Sync in
[Account → MyBrewFolio Sync](https://mybrewfolio.com/account/sync). No host binary, host-side key
generation, nested container, or incoming network port is required.

## Quick Start

Create a Compose project using [compose.headless.yaml](../compose.headless.yaml), or save this as
`compose.yaml`:

```yaml
services:
  sync:
    image: ghcr.io/modsmthng/mybrewfolio-sync:latest
    restart: unless-stopped
    environment:
      MYBREWFOLIO_SYNC_GAGGIMATE_HOST: gaggimate.local
    volumes:
      - sync-data:/data

volumes:
  sync-data:
```

1. Use your machine's fixed private LAN IP instead of `gaggimate.local` if Docker cannot resolve it.
2. Start the project: `docker compose up -d`.
3. Open its logs: `docker compose logs -f sync`. Follow the MyBrewFolio connection link and sign in.
4. Open **Account → MyBrewFolio Sync**, choose the matching preference and select **Save and start
   first sync**. Subsequent synchronization runs automatically.

The daemon polls for approval automatically. An unused link expires after ten minutes and is
renewed while the container remains unconnected. Closing the log window does not stop pairing or
Sync. The persistent volume retains the key, account connection and queue across container updates.

### Unraid

From an Unraid terminal, install the supplied [Unraid user template](../unraid/mybrewfolio-sync.xml) using the following command:

```sh
wget -P /boot/config/plugins/dockerMan/templates-user https://raw.githubusercontent.com/modsmthng/MyBrewFolio-Sync/refs/heads/main/unraid/mybrewfolio-sync.xml
```

From the Docker tab of Unraid, choose ***Add Container*** and select the ***MyBrewFolio-Sync*** template from the ***Templates*** drop-down. Enter your GaggiMate's LAN IP in the appropriate field and click ***Apply***.

Or, without using the supplied user template, choose **Add Container**
and enter the following settings. The template is provided in this repository; a Community Apps
listing is not required.

| Setting | Value |
|---|---|
| Repository | `ghcr.io/modsmthng/mybrewfolio-sync:latest` |
| Network | Bridge |
| Port mappings | None |
| Appdata path | `/mnt/user/appdata/mybrewfolio-sync` → `/data`, read/write |
| Environment variable | `MYBREWFOLIO_SYNC_GAGGIMATE_HOST` = your GaggiMate's LAN IP |
| Extra parameters | `--user=99:100` |

Use a dedicated appdata directory writable by UID 99 / GID 100. The template includes this user
override; the image otherwise uses UID/GID 10001. Start the container, open **Logs**, follow the
connection link, then use **WebUI** to manage Sync in MyBrewFolio. Enable Unraid's container autostart
if it should run after a reboot. No privileged mode or Docker socket mount is needed.

### Synology and other Docker hosts

In Synology Container Manager, create a **Project** and paste the Compose configuration above.
Start it and open the Sync container's **Log** view to connect your account. On hosts using a
container editor, use the same image, host variable, restart policy and persistent `/data` mount.

A named volume works with the image's default non-root user. For a bind-mounted shared folder,
grant the chosen UID/GID write access to that dedicated folder through the host's permission
settings and set Compose `user: "UID:GID"` when needed. No `PUID`/`PGID` variables or automatic
recursive permission changes are used. A permission error means `/data` is not writable by that
container user; do not solve it by making the private data directory world-writable.

## Two-way Notes Sync

Open **Account → MyBrewFolio Sync → Notes Sync**, choose the installation connected to your machine,
and select **Set up two-way Notes Sync**. Sync creates the complete **First Backup**. When the action
finishes, select **Review Notes activation**, compare both copies and confirm your choices.
MyBrewFolio Notes are preselected. Choosing an empty Note clears the other copy.

Only one installation writes Notes for a source. Another active writer is never taken over
automatically. You can turn the feature off in MyBrewFolio; ordinary GaggiMate-to-MyBrewFolio imports
continue. Later conflicts are reviewed in the affected Brew.

Create **Latest Backup**, download either backup, and preview a restore on the same Notes page.
A restore requires a fresh preview, selected Brews and explicit confirmation. It makes a new
Latest Backup before replacing machine Notes and skips Notes changed since the preview.

## Everyday Commands

Everyday actions are available in MyBrewFolio: **Synchronize now**, **Retry failed items**, **Check
status**, matching preferences and **Preview complete resync**. Keep the container running and
connected for machine actions. Waiting actions expire after ten minutes. Interrupted actions are
never replayed automatically; check the machine state, dismiss the action and review a new preview.

For local troubleshooting, the CLI is already inside the image:

```sh
docker compose exec sync mybrewfolio-syncd status
docker compose exec sync mybrewfolio-syncd diagnose
docker compose exec sync mybrewfolio-syncd help
docker compose exec sync mybrewfolio-syncd auth begin
docker compose exec sync mybrewfolio-syncd disconnect
```

Use `docker exec CONTAINER_NAME mybrewfolio-syncd ...` for a container managed through a NAS UI.
`diagnose` is read-only; its local output includes the machine address and troubleshooting guidance.
The web status check returns only availability and counts. After an intentional local disconnect or
server revocation, automatic pairing stays off until you explicitly run `auth begin` again.

### Optional terminal helper

On conventional Linux hosts, the optional installer asks for the machine address, creates Compose
configuration and a local shortcut, starts Docker, and shows how to read the automatic connection
link:

```sh
curl -fsSL https://raw.githubusercontent.com/modsmthng/MyBrewFolio-Sync/main/scripts/install-headless.sh | sh
~/.config/mybrewfolio-sync/sync status
~/.config/mybrewfolio-sync/sync diagnose
```

The shortcut delegates to the CLI inside the container. It is optional. Existing automation and the
interactive `sync notes enable` assistant remain supported. The assistant backs up before offering
bulk or custom decisions, defaults to MyBrewFolio Notes, and requires final confirmation. Run
`sync notes help` through your installed shortcut for the complete advanced command reference.

To update only an existing helper, preserving Compose, credentials, keys, volumes and the running
service:

```sh
curl -fsSL https://raw.githubusercontent.com/modsmthng/MyBrewFolio-Sync/main/scripts/install-headless.sh \
  | sh -s -- --update-helper
```

## Configuration and Recovery

Matching preferences and complete resync are managed in MyBrewFolio. A resync preview lists deleted
imports and unambiguous duplicate copies. Review the selected restores, merges and any differing
Notes before confirming; ambiguous matches remain unchanged. Advanced CLI commands such as
`resync preview` and `resync apply FILE --confirm` remain available for existing automation.

Change the machine's address in the container environment, then recreate the container. Keep
`MYBREWFOLIO_SYNC_GAGGIMATE_HOST` local; it is never sent to MyBrewFolio. If the environment variable
is omitted, the local `host set` command can persist the address in the data directory.

### Updating Docker

Use your NAS's image update/recreate action while preserving the `/data` mount. For Compose:

```sh
docker compose pull
docker compose up -d
```

For installations created by the optional installer, use its existing project file:

```sh
docker compose --project-directory ~/.config/mybrewfolio-sync -f ~/.config/mybrewfolio-sync/compose.yaml pull
docker compose --project-directory ~/.config/mybrewfolio-sync -f ~/.config/mybrewfolio-sync/compose.yaml up -d
```

Keep the existing project name and volume. Do not remove volumes during an update. Docker updates
are managed by the host; the daemon has no desktop updater. Older installations without web-control
support continue syncing and display an update notice in MyBrewFolio.

**Existing external keys:** configurations using `MYBREWFOLIO_SYNC_CREDENTIAL_KEY_FILE` continue to work.
Retain that setting and its original secret mount when updating. The daemon uses the supplied key;
it does not rotate or replace it. No migration or re-pairing is needed for these installations.

## Security, Networking, and Manual Automation

On first startup the daemon generates a random 32-byte `/data/state.key` with owner-only file
permissions. It encrypts the local OAuth credentials with that key and reuses it on every start.
The key is never logged or sent to MyBrewFolio. If encrypted credentials exist but the key is missing,
startup fails with a recovery instruction instead of silently replacing the key.

Back up the whole data directory, including its key, and protect the backup like an account
credential. Encryption with a key in the same volume does not protect against someone who can read
the entire volume. An external, read-only secret mount remains available for separate key storage.

The image runs as a non-root user. CLI commands use a local Unix socket to reach the running engine.
The daemon only makes outgoing authenticated HTTPS requests and local machine connections; no
public port, reverse proxy or public hostname for Sync is needed. Use `gaggimate.local` or a private
LAN IP. Arbitrary DNS hostnames and public machine addresses are rejected. Docker bridge networks
may not resolve mDNS; a fixed LAN IP is usually easiest.

For unattended installer configuration:

```sh
curl -fsSL https://raw.githubusercontent.com/modsmthng/MyBrewFolio-Sync/main/scripts/install-headless.sh \
  | sh -s -- --host 192.168.1.42 --non-interactive
```

The daemon still publishes its pairing link in logs; authorization requires a person to sign in.
`--no-start` only writes configuration and the optional helper. Set `MYBREWFOLIO_SYNC_HOME` on the
installer's `sh` command to choose another directory.
