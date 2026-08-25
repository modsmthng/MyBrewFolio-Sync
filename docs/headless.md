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

## Everyday Commands

All data commands write JSON to standard output. Logs and errors are written to standard error. `help`, `--help`, and `-h` work even while the daemon is stopped.

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

Notes Sync is one-way unless explicitly enabled. Its writing operations also use preview files and explicit confirmation:

```sh
~/.config/mybrewfolio-sync/sync notes backup
~/.config/mybrewfolio-sync/sync notes activate-preview > notes-decisions.json
~/.config/mybrewfolio-sync/sync notes activate BACKUP_ID notes-decisions.json --confirm
~/.config/mybrewfolio-sync/sync notes restore-preview BACKUP_ID
```

Run `~/.config/mybrewfolio-sync/sync notes help` for the complete Notes command reference.

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
