# MyBrewFolio Sync in Docker

The headless container uses the same synchronization engine as the desktop app, but does not require a desktop window, WebKit, or X11. Its persistent state lives in `/data` and its OAuth tokens are encrypted locally.

## Quick Start

You need Docker Engine or Docker Desktop with Docker Compose. Run this command from a terminal:

```sh
curl -fsSL https://raw.githubusercontent.com/modsmthng/MyBrewFolio-Sync/main/scripts/install-headless.sh | sh
```

The installer asks for the GaggiMate hostname (default: `gaggimate.local`), starts the service, and offers browser pairing immediately. Open the shown authorization URL, sign in to MyBrewFolio, and the installer waits for the connection automatically. You can safely choose not to pair yet.

The installer creates an owner-only local helper at `~/.config/mybrewfolio-sync/sync`. It is the normal way to manage the container; no global PATH change or Compose paths are required:

```sh
~/.config/mybrewfolio-sync/sync help
~/.config/mybrewfolio-sync/sync status
~/.config/mybrewfolio-sync/sync diagnose
```

## Two-way Notes Sync

To enable Notes synchronization in both directions, run this command in an interactive terminal:

```sh
~/.config/mybrewfolio-sync/sync notes enable
```

The assistant checks the current installation, creates a complete GaggiMate Notes activation
backup, and counts the differences for matching Brews. Choose **Keep MyBrewFolio Notes for all**,
**Use GaggiMate Notes for all**, **Custom**, or **Cancel**. For either bulk choice, no JSON file
is needed. The selected side wins even when its Note is empty, so review the summary before
answering `y` to `Enable two-way Notes Sync? [y/N]`.

If all matching Notes agree, only the final confirmation is needed. An already-enabled
installation does not create another backup. The assistant never takes over another
installation's active or pending Notes Sync. Cancellation does not enable synchronization or
change Notes; an activation already started may remain pending and its backup is retained.
Future Notes conflicts are reviewed in the affected Brew in MyBrewFolio.

### Custom choices

**Custom** does not activate synchronization. It creates a private `decisions.json` file inside
the container containing only differing `sourceKey` values and a `resolution` for each.
**MyBrewFolio is preselected**, just as on desktop. Change individual resolutions to `gaggimate`
where desired; keep the file as a JSON array:

```json
[
  { "sourceKey": "123:1735689600", "resolution": "mybrewfolio" },
  { "sourceKey": "124:1735689660", "resolution": "gaggimate" }
]
```

The assistant prints the exact backup ID and commands to copy the file to your host, return
the edited file with the correct container ownership, and activate it using `--confirm`.
Follow those generated commands; the example source keys above are not your Brew IDs.
No Note contents are included in this decision file or the assistant's output. Keep the file
private and remove the host file and generated container decision directory when finished.

### Existing Docker installations

The assistant is available starting with **0.4.7**. Once that image is published, first
update the container using the commands in [Updating Docker](#updating-docker). Then update
only the local helper:

```sh
curl -fsSL https://raw.githubusercontent.com/modsmthng/MyBrewFolio-Sync/main/scripts/install-headless.sh \
  | sh -s -- --update-helper
```

This does not change the Compose file, credentials, encryption key, volume, or running
container. For a custom installation directory, set `MYBREWFOLIO_SYNC_HOME` on the `sh`
command. SSH sessions need an interactive terminal (for example, `ssh -t`). Scripts without
a terminal should continue to use the explicit JSON commands below.

## Everyday Commands

Data commands write JSON to standard output. The interactive `notes enable` assistant writes
its prompts to standard error and reads from the terminal. Logs and errors are also written
to standard error. `help`, `--help`, and `-h` work even while the daemon is stopped.

```sh
# List every command, grouped by purpose
~/.config/mybrewfolio-sync/sync help

# Current status and a read-only explanation of what needs attention
~/.config/mybrewfolio-sync/sync status
~/.config/mybrewfolio-sync/sync diagnose

# Run a synchronization cycle now, change the GaggiMate address, or retry failures
~/.config/mybrewfolio-sync/sync sync-once
~/.config/mybrewfolio-sync/sync host set 192.168.1.42
~/.config/mybrewfolio-sync/sync retry

# Pair later, or remove the local account connection
~/.config/mybrewfolio-sync/sync auth begin
~/.config/mybrewfolio-sync/sync auth wait
~/.config/mybrewfolio-sync/sync disconnect
```

`diagnose` is read-only. It reports the connection, synchronized profile/shot/note counts, local queue and failure counts, conflicts, suppressed matches, duplicate policy, and concrete next commands.

## Updating Docker

Docker updates are manual. The desktop update check and restart prompt do not run in the headless container. From the installation directory created by the installer, pull the current image and recreate the service:

```sh
docker compose --project-directory ~/.config/mybrewfolio-sync -f ~/.config/mybrewfolio-sync/compose.yaml pull
docker compose --project-directory ~/.config/mybrewfolio-sync -f ~/.config/mybrewfolio-sync/compose.yaml up -d
```

If you chose a different installation directory, replace both paths with that directory. This reuses the installer-created Compose file, persistent data volume, encryption key, OAuth token, and configuration. It does not erase or replace local Sync state.

## Configuration and Recovery

The default duplicate policy is `reuse_matching`. It protects matching library entries from being imported a second time. For example, `10` synchronized shots and `350` suppressed entries means 10 new shots were synchronized while 350 matching entries were safely left unchanged. This is not a failed upload when `diagnose` reports no queue failures.

```sh
# Keep the safe default, or deliberately select duplicate imports for future matching items
~/.config/mybrewfolio-sync/sync configure reuse-matching
~/.config/mybrewfolio-sync/sync configure import-all

# Review suppressed items and other recovery choices without changing data
~/.config/mybrewfolio-sync/sync resync preview > decisions.json

# Review and explicitly edit decisions.json, then apply it with confirmation
~/.config/mybrewfolio-sync/sync resync apply decisions.json --confirm
```

Nothing is restored or imported automatically for suppressed matches. `resync preview` is the only recovery starting point; `resync apply` accepts only the reviewed JSON decision file plus `--confirm`.

Notes Sync is one-way unless explicitly enabled. Prefer the assistant under
[Two-way Notes Sync](#two-way-notes-sync). For scripted or advanced setup, the existing JSON
commands remain available:

```sh
~/.config/mybrewfolio-sync/sync notes activate-preview > notes-preview.json
# Build a decisions array from the differing items in the preview, and place
# the reviewed file in the container. Do not pass the entire preview object.
~/.config/mybrewfolio-sync/sync notes activate BACKUP_ID /data/decisions.json --confirm
~/.config/mybrewfolio-sync/sync notes backup
~/.config/mybrewfolio-sync/sync notes restore-preview BACKUP_ID
```

Run `~/.config/mybrewfolio-sync/sync notes help` for the complete Notes command reference.
Normal outgoing Notes updates do not create a Latest Backup automatically; run `sync notes backup`
whenever you want one. A restore still creates a fresh Latest Backup before it changes the machine.

## Security, Networking, and Manual Automation

The installer generates a 32-byte local encryption key at `~/.config/mybrewfolio-sync/state.key`, stores it with owner-only permissions, and mounts it into Docker as a secret. Do not delete or share this key while you want to retain the local container state. MyBrewFolio never receives the key or a refresh token.

The Compose setup uses a persistent Docker volume, restarts the daemon after a reboot, exposes no TCP port, and runs the image as a non-root user. While running, command invocations are forwarded through a local Unix socket inside the data volume, so no second synchronization process is started.

For non-interactive server automation, provide the host explicitly. Browser pairing is intentionally not attempted in this mode:

```sh
curl -fsSL https://raw.githubusercontent.com/modsmthng/MyBrewFolio-Sync/main/scripts/install-headless.sh \
  | sh -s -- --host 192.168.1.42 --non-interactive
```

`--no-start` writes the configuration and helper but does not start Docker. To use a different installation directory, set `MYBREWFOLIO_SYNC_HOME` before running the installer.

If `gaggimate.local` cannot be resolved from Docker's bridge network, use a fixed LAN IP address or local DNS/FQDN. On Linux, a deliberately configured host network is another option. The helper remains inside the installation directory in all cases.
