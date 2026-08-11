# Architecture and trust boundary

## Data flow

```text
GaggiMate on the private LAN
  |  history index, shot logs, notes and profiles
  v
MyBrewFolio Sync desktop companion
  |  private-host validation, independent parsing, local retry queue
  |  HTTPS with OAuth access token
  v
MyBrewFolio Sync API
```

The companion never sends the local hostname, IP address, or a GaggiMate hardware identifier to
MyBrewFolio. It never writes shots or profiles, selects profiles, favorites profiles, or deletes
anything on the machine. Only explicitly enabled two-way Notes synchronization can write a Notes
object for an exact mapped shot.

The app can locally hide its normal application icon while retaining the tray entry point. macOS
uses Tauri's accessory activation policy and Dock visibility API; Windows and Linux use the main
window's skip-taskbar capability. The preference is stored in local SQLite, survives account
disconnect, and is applied before the background loop starts.

OAuth tokens are stored in the operating-system keychain. SQLite stores settings, cached server
state, source hashes, and validated content waiting for an upload retry.

## Synchronization schedule

- The shot index is checked every 30 seconds.
- New or changed shots are parsed from `.slog` files and queued with their notes.
- Profiles are compared every five minutes through the GaggiMate profile WebSocket protocol.
- Notes for recent shots are refreshed every five minutes.
- A throttled full notes pass runs once per day.
- Notes are read through `req:history:notes:get` with the ordinary GaggiMate history ID. Empty
  objects, null/missing notes payloads and the machine's protocol-level “not found” response mean
  that no notes exist.
- Validated data remains in the local queue while the internet or MyBrewFolio is unavailable.

Shots and profiles are one-way. Deleting a synchronized object in MyBrewFolio suppresses its
automatic reimport but does not modify the GaggiMate.

Two-way Notes synchronization is off by default and is bound to one active writer installation.
Activation requires a complete, finalized machine-Notes backup, shown as **First Backup** in the
interface. Differences are shown before the
first write and existing MyBrewFolio Notes are preselected. The server issues short-lived,
idempotent operations containing the expected machine hash. The app reads and compares the current
machine state before `req:history:notes:save`, reads it back after the write, and acknowledges only
the verified target hash. A changed precondition becomes a conflict rather than an overwrite.
Before later outbound batches or a restore, the app replaces the backup shown as **Latest Backup**;
the protected first backup remains separate. Disabling the feature invalidates outstanding server
leases.

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
| `POST/DELETE /v1/sync/notes/two-way/*` | Request, activate, or immediately disable the optional writer permission |
| `POST /v1/sync/notes/backups/*` | Upload/finalize the activation or latest Notes backup in bounded chunks |
| `POST /v1/sync/notes/outbound/*` | Claim and acknowledge short-lived compare-before-write operations |
| `GET/POST /v1/sync/notes/backups/:id/*` | Read backup contents and report verified restore results |
| `POST /v1/sync/heartbeat` | Report app and machine availability without a local address |
| `POST /v1/sync/conflicts/:itemId/resolve` | Resolve a synchronization conflict |
| `DELETE /v1/sync/devices/:id` | Disconnect an installation |

The hosted API implementation, database schema, website, and infrastructure are intentionally not
part of this repository.

## Releases

Pull requests and ordinary pushes run frontend fixtures and native Rust checks. A tag matching
`vMAJOR.MINOR.PATCH` builds draft installers for Windows, macOS, and Linux. Release update artifacts
are signed with a protected key available only to the owner-controlled release job. Each platform
job also uploads a stable user-facing alias for the current DMG, MSI, AppImage, or DEB package.
The MyBrewFolio Support page links these aliases through GitHub's `releases/latest/download` route,
while updater-only `.sig` files remain outside the normal installation flow.

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
