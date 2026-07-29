# MyBrewFolio Sync

MyBrewFolio Sync is the open-source desktop companion for copying shots, profiles, and notes from
one local GaggiMate to a private MyBrewFolio library. It is intentionally one-way: the companion
never selects, edits, or deletes anything on the machine.

## User flow

1. Install MyBrewFolio Sync.
2. Choose **Connect MyBrewFolio** and confirm sign-in in the normal browser.
3. Confirm the detected `gaggimate.local` address, or enter a private local IP.

The application then starts with the computer, checks for new shots every 30 seconds, compares
profiles every five minutes, and catches up after either the computer, machine, or internet was
offline. Visible Sync actions and background synchronization show a stable activity indicator so
the user can see when work is still in progress.

Before its first import, Sync asks whether matching GaggiMate shots already in MyBrewFolio should
be reused. **Complete resync** can later scan the whole machine, preview recoverable deleted
machine content and safe duplicate merges, then apply only the user's confirmed choices.
After applying a resync, Sync refreshes the authoritative cloud state before rebuilding its local
scan so restored items are imported without disconnecting the account.
When suppressed items need attention, the app directs users first to MyBrewFolio.com. Under
**Account → MyBrewFolio Sync**, every suppressed import can be allowed again with one confirmed
bulk action.

## Code quality
https://sonarcloud.io/project/overview?id=modsmthng_MyBrewFolio-Sync

## Local development

Prerequisites are Node.js 24, the stable Rust toolchain, and the platform packages required by
Tauri 2.

```bash
npm ci
npm run test:fake-gaggimate
npm run tauri:dev
```

The manually started fake machine listens on `127.0.0.1:8088` and provides a real binary shot
index, a version-five `.slog`, notes JSON, and the profile WebSocket protocol:

```bash
npm run fake-gaggimate
```

Use `127.0.0.1:8088` as the machine address in a development build.

The automated fixture test uses `127.0.0.1:18088` so it cannot collide with a manually running
development machine.

## Build-time configuration

The companion contains no client secret. These public values are compiled into release builds:

```dotenv
MYBREWFOLIO_SYNC_API_URL=https://mybrewfolio.com
MYBREWFOLIO_SYNC_OAUTH_CLIENT_ID=<public Clerk OAuth client ID>
MYBREWFOLIO_SYNC_AUTHORIZE_URL=https://clerk.mybrewfolio.com/oauth/authorize
MYBREWFOLIO_SYNC_TOKEN_URL=https://clerk.mybrewfolio.com/oauth/token
MYBREWFOLIO_SYNC_UPDATER_PUBLIC_KEY=<Tauri updater public key>
```

OAuth tokens are stored in the operating-system keychain. SQLite stores settings, the local
offline/retry queue, cached server state, and bounded diagnostics without notes contents,
credentials, or the machine address.

## Verification

```bash
npm run build
npm run test:fake-gaggimate
cd src-tauri
cargo fmt --check
cargo check --locked
cargo test --locked --lib
```

The trust boundary, synchronization behavior, public API contract, and release process are described
in [docs/architecture.md](docs/architecture.md).

The supplied logo masters live in `assets/`. Rebuild and verify all native, tray, and Microsoft
Store icon formats reproducibly with:

```bash
./scripts/generate-icons.sh
```

The monochrome `assets/tray-template.svg` is rendered separately to
`src-tauri/icons/tray-template.png`. macOS treats it as a template image so the menu-bar icon stays
visible in both Light and Dark appearances. Microsoft Store listing artwork is kept in
`assets/microsoft-store/` at the exact requested 72, 150 and 300 pixel sizes.

## Security and privacy

- OAuth tokens are stored in the operating-system keychain.
- The GaggiMate hostname or local IP remains on the computer.
- Support and privacy links use a fixed allowlist and open in the operating system's browser.
- Only explicitly synchronized library content is sent to the MyBrewFolio Sync API.
- The companion permits only loopback and private-network machine targets.
- Release update metadata is signed. The private signing key is never stored in this repository.
- Windows GitHub MSI releases use the signed MyBrewFolio updater. Microsoft Store MSIX releases use
  Microsoft Store updates instead.
- Disconnect always removes the local Sync connection after confirmation. If immediate server
  revocation is unavailable, the app links directly to Account → Sync for manual revocation.
- Microsoft Store submission packages are kept in separate `store-vX.Y.Z` draft releases. These
  drafts are for Partner Center submission only and must never be published. Each draft contains
  the unsigned Partner Center MSIX and a temporary self-signed test bundle for an exact local
  Windows installation test. Store packaging declares the required Microsoft Visual C++ framework
  dependency and verifies the executable subsystem, package identity and embedded files.

Please report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.
By contributing, you agree that your contribution is licensed under GPL-3.0-or-later.
