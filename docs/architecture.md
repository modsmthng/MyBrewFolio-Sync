# Architecture and trust boundary

## Data flow

```text
GaggiMate on the private LAN
  |  history index, shot logs, notes and profiles
  v
Shared Rust SyncEngine
  |  private-host validation, independent parsing, SQLite state and retry queue
  |\
  | \-- Desktop adapter: Tauri UI, tray, deep links and OS keychain
  |\
  | \-- Headless adapter: mybrewfolio-syncd, local Unix socket and encrypted credentials
  |  HTTPS with OAuth access token
  v
MyBrewFolio Sync API
```

The desktop app and `mybrewfolio-syncd` use the same engine, local data model, GaggiMate protocol
client, queue behaviour, and MyBrewFolio Sync API. The adapters only provide their runtime-specific
interfaces and credential storage; this prevents the headless variant from becoming a second sync
implementation.

Neither runtime sends the local hostname, IP address, or a GaggiMate hardware identifier to
MyBrewFolio. Automatic synchronization never writes shots or profiles and never deletes anything on
the machine. Two explicit user actions may write: two-way Notes synchronization for an exact mapped
shot, and the Profile Store bridge for one confirmed profile. The latter may save, favorite and
select that profile but never deletes or reorders profiles.

The app can locally hide its normal application icon while retaining the tray entry point. macOS
uses Tauri's accessory activation policy and Dock visibility API; Windows and Linux use the main
window's skip-taskbar capability. The preference is stored in local SQLite, survives account
disconnect, and is applied before the background loop starts.

Autostart launches carry an internal `--autostart` argument. The initial window stays hidden until
the tray is ready for that launch mode, while manual starts and first-run setup explicitly show it.
The Store package uses a small startup launcher because MSIX startup tasks cannot pass arguments to
the main executable.

The desktop adapter stores OAuth tokens in the operating-system keychain. The headless adapter
stores them in an encrypted file in its data directory; its 32-byte encryption key is supplied from
a key file (normally mounted as a Docker secret) and is never sent to MyBrewFolio. SQLite stores
settings, cached server state, source hashes, and validated content waiting for an upload retry in
both runtimes. A running daemon accepts local CLI requests through a Unix socket only; it does not
open a network port.

## Synchronization schedule

- The shot index is checked every 30 seconds.
- New or changed shots are parsed from `.slog` files and queued with their notes.
- Profiles are compared every five minutes through the GaggiMate profile WebSocket protocol.
- Profile Store operations are checked every 30 seconds and accepted only when addressed to this
  installation's authenticated device ID. Inventory, fetch, preview and install results use
  short-lived leases; installation reloads the profile before reporting success.
- Notes for recent shots are refreshed every five minutes.
- A throttled full notes pass runs once per day.
- Notes are read through `req:history:notes:get` with the ordinary GaggiMate history ID. Empty
  objects, null/missing notes payloads and the machine's protocol-level “not found” response mean
  that no notes exist.
- Validated data remains in the local queue while the internet or MyBrewFolio is unavailable.

Shots and profiles are one-way. Deleting a synchronized object in MyBrewFolio suppresses its
automatic reimport but does not modify the GaggiMate.

## Two-way Notes synchronization

Two-way Notes synchronization is off by default and is bound to one active writer installation.
Activation starts with a complete, finalized machine-Notes backup, shown as **First Backup** in the
desktop interface. The client then obtains an activation preview containing the differences and the
proposed decisions; existing MyBrewFolio Notes are preselected. A user must review those decisions
before writing is enabled. In the headless CLI, activation accepts only that reviewed JSON decision
file and an explicit `--confirm`; the desktop shows the equivalent confirmation in its interface.

When Notes Sync is active, the server issues short-lived, idempotent operations containing the
expected machine hash. Before `req:history:notes:save`, the client reads and compares the current
machine Notes object; it reads it again after the write and acknowledges only the verified target
hash. A changed precondition becomes a conflict rather than an overwrite. Before later outbound
batches or a restore, the client replaces the backup shown as **Latest Backup**; the protected
First Backup remains separate. Restore is also preview-first and only applies selected backup items
after explicit confirmation. Disabling Notes Sync immediately invalidates outstanding writer
leases, so no further machine writes are scheduled.

Before the first import, the source stores whether exact GaggiMate-ID/recording-time matches reuse
existing MyBrewFolio shots. Complete resync builds a read-only preview from a fresh local inventory.
Only a confirmed apply can clear suppressions for still-present machine objects or merge an
unambiguous later Sync copy into the older MyBrewFolio shot. Differing notes require an explicit
choice; ambiguous matches remain untouched. After apply, the client discards the old scan state
and refreshes the authoritative server state before uploading the restored inventory.

## Public server contract

The companion uses only authenticated endpoints below `/v1/sync`:

| Endpoint | Purpose |
|---|---|
| `POST /v1/sync/devices` | Register an OAuth-authorized installation |
| `GET /v1/sync/state` | Read known mappings, conflicts, and suppressions |
| `PUT /v1/sync/settings` | Save duplicate policy and initialize first sync |
| `POST /v1/sync/resync/preview` | Preview recoverable items and duplicate candidates |
| `POST /v1/sync/resync/apply` | Apply selected restores and validated merges transactionally |
| `POST /v1/sync/batches` | Submit bounded, validated synchronization batches |
| `POST /v1/sync/notes/two-way/request`, `POST /activate`, `DELETE /v1/sync/notes/two-way` | Request, activate, or immediately disable the optional writer permission |
| `POST /v1/sync/notes/backups`, `POST /:id/items`, `POST /:id/finalize` | Create and finalize the First or Latest Notes backup in bounded chunks |
| `GET /v1/sync/notes/activation-preview/:id` | Return the activation differences and proposed user decisions |
| `POST /v1/sync/notes/outbound/claim`, `POST /:id/result` | Claim and acknowledge short-lived compare-before-write operations |
| `GET /v1/sync/notes/backups/:id/items`, `POST /:id/restore-results` | Read backup contents and report verified restore results |
| `POST /v1/sync/heartbeat` | Report app and machine availability without a local address |
| `POST /v1/sync/conflicts/:itemId/resolve` | Resolve a synchronization conflict |
| `DELETE /v1/sync/devices/:id` | Disconnect an installation |

The hosted API implementation, database schema, website, and infrastructure are intentionally not
part of this repository.

## Releases

Pull requests and ordinary pushes run frontend fixtures and native Rust checks. A tag matching
`vMAJOR.MINOR.PATCH` builds draft installers for Windows, macOS, and Linux, plus a multi-architecture
headless image for `linux/amd64` and `linux/arm64` at
`ghcr.io/modsmthng/mybrewfolio-sync`. The release tag is published as the matching image tag and as
`latest`. Release update artifacts are signed with a protected key available only to the
owner-controlled release job. Each platform job also uploads a stable user-facing alias for the
current DMG, MSI, AppImage, or DEB package. The MyBrewFolio Support page links these aliases through
GitHub's `releases/latest/download` route, while updater-only `.sig` files remain outside the normal
installation flow.

Windows has two deliberately separate update channels. The direct GitHub MSI build uses the signed
MyBrewFolio updater and `latest.json`. The manual `Microsoft Store package` workflow builds the
MSIX with `MYBREWFOLIO_SYNC_WINDOWS_STORE_BUILD=true`; that package delegates updates to Microsoft
Store. A version input must match `package.json`, `tauri.conf.json` and `Cargo.toml`. The workflow is
limited to 30 minutes and uploads only to a separate `store-vX.Y.Z` draft release. This draft is
visible only to repository collaborators with push access and must never be published. The normal
`vX.Y.Z` release therefore contains only public direct-download and updater assets. The MSIX
manifest registers `mybrewfolio-sync://` so the OAuth return path works in the Store build.
The Store manifest declares `Microsoft.VCLibs.140.00.UWPDesktop`. Packaging verifies the Visual C++
dependency, Windows GUI subsystem, package identity and embedded payload before upload. The private
draft also contains an equivalent self-signed test MSIX, its short-lived public certificate, the
matching UWPDesktop VCLibs framework and install/removal scripts in a ZIP. The bundle builder reads
the framework Appx manifest and rejects generic or wrong-architecture VCLibs packages before upload.
The installer repeats that identity check, compares the bundled
certificate with the MSIX signer, requests elevation only to add it temporarily to Local Computer
→ Trusted People, verifies a valid signature and then registers the package for the original test
user. The removal script deletes both package and certificate. Only the unsigned MSIX is submitted
to Partner Center. Startup diagnostics record app, OS and WebView2 versions and frontend readiness
in the app data directory without recording tokens, machine addresses or synchronized content.
Support, privacy and account-management links are restricted to a fixed MyBrewFolio URL allowlist
and open in the operating system browser. Disconnect is local-first after explicit confirmation:
the app clears local account state even when the server cannot be reached, and then directs the
user to Account → Sync when server-side revocation still needs to be completed.

The desktop interface keeps connectivity separate from activity: its header reports only connected
or not connected, while one fixed status line carries operations, short-lived success messages and
persistent failures. Shot and Notes conflicts are resolved from the affected Brew in the hosted
Analyzer; profile conflicts and suppressed objects remain under Account → Sync → Not synchronized.

Required GitHub repository configuration:

| Type | Name |
|---|---|
| Variable | `MYBREWFOLIO_SYNC_OAUTH_CLIENT_ID` |
| Variable | `MYBREWFOLIO_SYNC_UPDATER_PUBLIC_KEY` |
| Secret | `TAURI_SIGNING_PRIVATE_KEY` |
| Secret | `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` |
| Variable | `MYBREWFOLIO_MSIX_IDENTITY_NAME` |
| Variable | `MYBREWFOLIO_MSIX_PUBLISHER` |

Release signing secrets are unavailable to workflows triggered from forks. Releases remain drafts
until their installers and signed updater metadata have been reviewed.
