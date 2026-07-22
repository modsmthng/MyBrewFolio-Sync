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
MyBrewFolio. It never writes to, selects, favorites, or deletes anything on the machine.

OAuth tokens are stored in the operating-system keychain. SQLite stores settings, cached server
state, source hashes, and validated content waiting for an upload retry.

## Synchronization schedule

- The shot index is checked every 30 seconds.
- New or changed shots are parsed from `.slog` files and queued with their notes.
- Profiles are compared every five minutes through the GaggiMate profile WebSocket protocol.
- Notes for recent shots are refreshed every five minutes.
- A throttled full notes pass runs once per day.
- Validated data remains in the local queue while the internet or MyBrewFolio is unavailable.

Synchronization is one-way. Deleting a synchronized object in MyBrewFolio suppresses its automatic
reimport but does not modify the GaggiMate.

## Public server contract

The companion uses only authenticated endpoints below `/v1/sync`:

| Endpoint | Purpose |
|---|---|
| `POST /v1/sync/devices` | Register an OAuth-authorized installation |
| `GET /v1/sync/state` | Read known mappings, conflicts, and suppressions |
| `POST /v1/sync/batches` | Submit bounded, validated synchronization batches |
| `POST /v1/sync/heartbeat` | Report app and machine availability without a local address |
| `POST /v1/sync/conflicts/:itemId/resolve` | Resolve a synchronization conflict |
| `DELETE /v1/sync/devices/:id` | Disconnect an installation |

The hosted API implementation, database schema, website, and infrastructure are intentionally not
part of this repository.

## Releases

Pull requests and ordinary pushes run frontend fixtures and native Rust checks. A tag matching
`vMAJOR.MINOR.PATCH` builds draft installers for Windows, macOS, and Linux. Release update artifacts
are signed with a protected key available only to the owner-controlled release job.

Required GitHub repository configuration:

| Type | Name |
|---|---|
| Variable | `MYBREWFOLIO_SYNC_OAUTH_CLIENT_ID` |
| Variable | `MYBREWFOLIO_SYNC_UPDATER_PUBLIC_KEY` |
| Secret | `TAURI_SIGNING_PRIVATE_KEY` |
| Secret | `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` |
| Variable | `MYBREWFOLIO_MSIX_IDENTITY_NAME` (optional) |
| Variable | `MYBREWFOLIO_MSIX_PUBLISHER` (optional) |

Release signing secrets are unavailable to workflows triggered from forks. Releases remain drafts
until their installers and signed updater metadata have been reviewed.

