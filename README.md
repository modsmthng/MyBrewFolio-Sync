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
offline.

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

OAuth tokens are stored in the operating-system keychain. SQLite stores only settings, the local
offline queue, and cached server state.

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

## Security and privacy

- OAuth tokens are stored in the operating-system keychain.
- The GaggiMate hostname or local IP remains on the computer.
- Only explicitly synchronized library content is sent to the MyBrewFolio Sync API.
- The companion permits only loopback and private-network machine targets.
- Release update metadata is signed. The private signing key is never stored in this repository.

Please report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.
By contributing, you agree that your contribution is licensed under GPL-3.0-or-later.
