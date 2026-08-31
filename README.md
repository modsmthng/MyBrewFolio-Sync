# MyBrewFolio Sync

MyBrewFolio Sync is the open-source desktop companion for copying shots, profiles, and notes from
one local GaggiMate to a private MyBrewFolio library. Automatic shot and profile synchronization is
one-way. Users may separately enable two-way Notes synchronization after a full machine-Notes backup
and review. A separately confirmed Profile Store installation may save, favorite and select only the
chosen Store profile; the companion never edits/deletes shots on the machine.

MyBrewFolio is a platform for smart espresso machines.
Track and share brews, quickly complete brew notes with smart suggestions from your beans and grinder library, discover community profiles, and move from simple overviews to advanced phase analysis and statistics. Built for GaggiMate brews and profiles today. More coffee machine integrations are planned for the future. 

### https://mybrewfolio.com/info  

## User flow

1. Install MyBrewFolio Sync.
2. Choose **Connect MyBrewFolio** and confirm sign-in in the normal browser.
3. Confirm the detected `gaggimate.local` address, or enter a private local IP.

The application then starts with the computer in the background, checks for new shots every 30
seconds, compares profiles every five minutes, and catches up after either the computer, machine,
or internet was offline. Profile Store requests use their own outgoing wake-up channel, so they do
not wait for that normal 30-second cycle. Open Sync from its menu bar or tray icon; manually launched and first-run
windows still open normally. One fixed status line reports the current operation and retains
actionable failures without replacing the separate connection indicator.

The optional **Hide app icon from Dock or taskbar** setting keeps the menu bar or tray icon as the
permanent entry point. On macOS it uses accessory-app mode to hide Sync from the Dock and app
switcher. On Windows and supported Linux desktops it hides the window from the taskbar or dock.

Before its first import, Sync asks whether matching GaggiMate shots already in MyBrewFolio should
be reused. **Complete resync** can later scan the whole machine, preview recoverable deleted
machine content and safe duplicate merges, then apply only the user's confirmed choices.
After applying a resync, Sync refreshes the authoritative cloud state before rebuilding its local
scan so restored items are imported without disconnecting the account.
When items were not synchronized, the app directs users first to MyBrewFolio.com. Under
**Account → MyBrewFolio Sync → Not synchronized**, every suppressed import can be allowed again
with one confirmed bulk action. Shot and Notes conflicts are reviewed in the affected Brew; profile
conflicts remain on the Not synchronized page.

Optional **Two-way Notes Sync** is limited to exact, already mapped GaggiMate shots. Activation
creates the protected **First Backup** first. If existing MyBrewFolio and GaggiMate Notes differ,
MyBrewFolio is preselected and every choice remains editable before any machine write. The app
rechecks the machine copy immediately before writing, verifies it afterwards, and creates a
**Latest Backup** before later outgoing write batches. Both backup slots can be downloaded on the
MyBrewFolio Account page. Restore is preview-first and applies only the explicitly confirmed
selection from the desktop app or headless CLI.

## Code quality
https://sonarcloud.io/summary/new_code?id=modsmthng_MyBrewFolio-Sync&branch=main 

## Local development

Prerequisites are Node.js 24, the stable Rust toolchain, and the platform packages required by
Tauri 2.

```bash
npm ci
npm run test:fake-gaggimate
npm run tauri:dev
```

The manually started fake machine listens on `127.0.0.1:8088` and provides a real binary shot
index, a version-six `.slog`, notes JSON, and the profile WebSocket protocol. The parser also
retains compatibility with older v1-v5 shot files:

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
MYBREWFOLIO_SYNC_DEVICE_CALLBACK_URL=https://mybrewfolio.com/v1/sync/device-auth/callback
MYBREWFOLIO_SYNC_UPDATER_PUBLIC_KEY=<Tauri updater public key>
```

OAuth tokens are stored in the operating-system keychain. SQLite stores settings, the local
offline/retry queue, cached server state, and bounded diagnostics without notes contents,
credentials, or the machine address.

## Headless Linux and Docker

`mybrewfolio-syncd` is a Tauri-free Linux binary that shares the desktop application's SyncEngine.
It provides daemon, one-shot, authentication, status, configuration, Notes, and resync commands
as JSON-producing CLI operations. The Docker installer asks for the local GaggiMate host, starts
pairing in the browser, and creates a local `sync` helper, so users do not need to manage Compose
paths. Docker state is stored under `/data`; tokens are encrypted locally with an installer-created
32-byte key mounted as a Docker secret instead of an OS keychain. See
[docs/headless.md](docs/headless.md) for installation, everyday commands, browser pairing, and LAN
networking guidance.

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
visible in both Light and Dark appearances. Windows and Linux use the generated, high-contrast
`src-tauri/icons/tray-color.png`. Microsoft Store listing artwork is kept in
`assets/microsoft-store/` at the exact requested 72, 150 and 300 pixel sizes.

## Security and privacy

- Desktop OAuth tokens are stored in the operating-system keychain. The headless runtime stores
  them in an encrypted local file whose 32-byte key is mounted as a Docker secret.
- The GaggiMate hostname or local IP remains on the computer.
- Support and privacy links use a fixed allowlist and open in the operating system's browser.
- Only explicitly synchronized library content is sent to the MyBrewFolio Sync API.
- The companion permits only loopback and private-network machine targets.
- Release update metadata is signed. The private signing key is never stored in this repository.
- Windows GitHub MSI releases use the signed MyBrewFolio updater. Microsoft Store MSIX releases use
  Microsoft Store updates instead.
- Disconnect always removes the local Sync connection after confirmation. If immediate server
  revocation is unavailable, the app links directly to Account → Sync for manual revocation.
- Dock/taskbar visibility is a local app preference. Hiding the main app icon never removes the
  menu bar or tray icon and does not affect background synchronization.
- Microsoft Store submission packages are kept in separate `store-vX.Y.Z` draft releases. These
  drafts are for Partner Center submission only and must never be published. Each draft contains
  the unsigned Partner Center MSIX and a temporary self-signed test bundle for an exact local
  Windows installation test. Store packaging declares the required Microsoft Visual C++ framework
  dependency and verifies the executable subsystem, package identity and embedded files. Store
  packages are rebuilt independently through the manual `Microsoft Store package` workflow, so
  testing never changes an already published GitHub release.

Please report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.
By contributing, you agree that your contribution is licensed under GPL-3.0-or-later.
