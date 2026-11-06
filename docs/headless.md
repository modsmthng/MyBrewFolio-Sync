# MyBrewFolio Sync in Docker

Run Sync on a Linux host or NAS to synchronize your local GaggiMate machines with MyBrewFolio. The container connects through your browser and stores its connection and queue in `/data`. No incoming port is required.

For Home Assistant OS, use the [Home Assistant app](../home-assistant/mybrewfolio_sync/DOCS.md). Home Assistant Container users can use Docker.

## Quick start

Create a Compose project with [compose.headless.yaml](../compose.headless.yaml), or save this as `compose.yaml`:

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

1. If Docker cannot resolve `gaggimate.local`, replace it with the machine's fixed private LAN IP.
2. Run `docker compose up -d`, then open the logs with `docker compose logs -f sync`.
3. Follow the connection link in the logs and sign in to MyBrewFolio. The link renews automatically if unused.
4. If your account has no machines, Sync creates the first one using the configured address. If it already has machines, [choose or add a machine](#multiple-machines) explicitly.
5. Open [Account → MyBrewFolio Sync](https://mybrewfolio.com/account/sync), choose the machine's import preference and select **Save and start first sync**.

Keep the container and its `/data` volume. Closing the logs does not stop Sync. Account connection alone does not start the first import; you must save the import preference.

## Unraid

Install the supplied [Unraid template](../unraid/mybrewfolio-sync.xml) from an Unraid terminal:

```sh
wget -P /boot/config/plugins/dockerMan/templates-user https://raw.githubusercontent.com/modsmthng/MyBrewFolio-Sync/refs/heads/main/unraid/mybrewfolio-sync.xml
```

In the Docker tab, choose **Add Container**, select **MyBrewFolio-Sync** from **Templates**, enter the GaggiMate LAN IP and select **Apply**. Open the container logs and follow the connection link, then finish setup in MyBrewFolio. Enable container autostart if desired.

Alternatively configure the image `ghcr.io/modsmthng/mybrewfolio-sync:latest` on a bridge network with no ports, a dedicated writable appdata folder mounted at `/data`, `MYBREWFOLIO_SYNC_GAGGIMATE_HOST` set to the LAN IP, and `--user=99:100`. The template supplies this user override; ensure the appdata folder is writable by UID 99 / GID 100.

## Synology and other Docker hosts

In Synology Container Manager, create a **Project** with the Compose configuration above. Start it, open the container's **Log** view and follow the connection link. Other container editors need the same image, host variable, restart policy and persistent `/data` mount.

A named volume works with the image's default UID/GID `10001:10001`. For a bind-mounted folder, grant the container user access to the dedicated folder and its existing files. If needed, set `user: "UID:GID"` in Compose. A `/data` permission error can also indicate a read-only mount or insufficient disk space; do not make private data world-writable. Sync does not change existing ownership or permissions.

## Multiple machines

If logs say **Account connected; machine selection required**, sign-in succeeded but you must select a machine. For Compose, list account machines and attach one using its ID and local address:

```sh
docker compose exec sync mybrewfolio-syncd machines list
docker compose exec sync mybrewfolio-syncd machines add --existing MACHINE_ID HOST
```

To create a new account machine instead, use `docker compose exec sync mybrewfolio-syncd machines add "Machine name" HOST`. Repeat for additional machines. Names must be 1–24 characters and unique in the account, ignoring case. The configured host is used automatically on a new installation only if the account has no machines.

Manage connections with `machines rename MACHINE_ID "New name"` or `machines remove MACHINE_ID` using the same `docker compose exec sync mybrewfolio-syncd` prefix. Removing a connection leaves its MyBrewFolio history and machine content intact; use `add --existing` to reconnect it. Account disconnect affects every machine on this installation.

When several machines are connected, machine-specific commands need `--machine MACHINE_ID`, for example:

```sh
docker compose exec sync mybrewfolio-syncd host set 192.168.1.45 --machine MACHINE_ID
docker compose exec sync mybrewfolio-syncd diagnose --machine MACHINE_ID
```

With one machine, the selection can be omitted. `machines` commands take an ID directly. Change the first machine's address in the container environment and recreate the container; if the variable is omitted, `host set` can save it locally. Additional addresses are stored in `/data`.

## Everyday use and troubleshooting

Use MyBrewFolio to synchronize now, review conflicts, retry items, change matching and preview a complete resync. Keep the container running and connected for these actions. Interrupted actions are not replayed automatically; check the machine state and review a new preview.

For local troubleshooting:

```sh
docker compose exec sync mybrewfolio-syncd status
docker compose exec sync mybrewfolio-syncd diagnose
docker compose exec sync mybrewfolio-syncd help
```

On a NAS without Compose, use `docker exec CONTAINER_NAME mybrewfolio-syncd ...`. Local `diagnose` output includes the machine address. If a machine cannot be reached, check its private IP, network routing and internet access; Sync retries and resumes on its configured schedule. `connected: true` in status means the account is connected, even if the machine list is empty.

After an intentional disconnect or server revocation, restart pairing explicitly with `docker compose exec sync mybrewfolio-syncd auth begin`. The `disconnect` command disconnects the account locally.

## Notes and recovery

In **Account → MyBrewFolio Sync**, select a machine and open **Notes Sync** to enable optional two-way Notes. Sync creates a **First Backup** before you review and confirm the differences. MyBrewFolio Notes are preselected; choosing an empty Note clears the other copy. Only one installation can write Notes for a machine.

You can create and download a **Latest Backup** or preview a restore on the same page. A restore requires a fresh preview, selected Brews and confirmation; it creates a new Latest Backup before writing. Turning Notes Sync off leaves ordinary imports running. Review later Notes conflicts in the affected Brew.

## Updates and backups

Update through your NAS's image update/recreate action, or with Compose:

```sh
docker compose pull
docker compose up -d
```

Preserve the existing project and `/data` volume; do not remove volumes during an update. Back up the **entire data directory including `state.key`**, protect it like account credentials, and restore it together. The key is stored alongside the encrypted credentials by default, so anyone who can read the complete volume can access them.

If you use `MYBREWFOLIO_SYNC_CREDENTIAL_KEY_FILE`, keep its original setting and external key mount during updates and recovery. Moving to an empty volume or host folder creates a new local installation; to retain the existing one, stop Sync and copy all data and its key with private permissions. If you intentionally start empty, pair again and select the existing account machine.

## Optional terminal helper

On a conventional Linux host, the installer can create Compose configuration and a local `sync` shortcut:

```sh
curl -fsSL https://raw.githubusercontent.com/modsmthng/MyBrewFolio-Sync/main/scripts/install-headless.sh | sh
~/.config/mybrewfolio-sync/sync status
~/.config/mybrewfolio-sync/sync diagnose
```

The installer shows how to open the connection link. Use `sync notes help` for advanced Notes commands. To update only the helper without changing the running service or its data:

```sh
curl -fsSL https://raw.githubusercontent.com/modsmthng/MyBrewFolio-Sync/main/scripts/install-headless.sh | sh -s -- --update-helper
```

For unattended configuration, pass `--host 192.168.1.42 --non-interactive` after `sh -s --`; account authorization still requires a person. `--no-start` only writes configuration and the helper. Set `MYBREWFOLIO_SYNC_HOME` to choose another directory. To update an installer-created service with Compose, use its existing project and file: `docker compose --project-directory ~/.config/mybrewfolio-sync -f ~/.config/mybrewfolio-sync/compose.yaml pull`, followed by the same command ending in `up -d`.
