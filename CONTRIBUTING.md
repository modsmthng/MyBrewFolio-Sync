# Contributing

Contributions are welcome. Open an issue first for substantial behavior or protocol changes.

## Development

Install Node.js 24, the stable Rust toolchain, and the platform dependencies for Tauri 2. Then run:

```sh
npm ci
npm run tauri:dev
```

For development without a physical machine, start the fake GaggiMate and use `127.0.0.1:8088` as the machine address:

```sh
npm run fake-gaggimate
```

Release builds use public build-time OAuth and API settings; see `src-tauri/src/cloud.rs` and the [release workflow](.github/workflows/release.yml). Never add a client secret to the companion.

## Verification

Add or update tests for parser, protocol, storage, or synchronization changes. Run the checks relevant to your change; the full desktop verification is:

```sh
npm run build
npm run test:fake-gaggimate
cd src-tauri
cargo fmt --check
cargo check --locked
cargo test --locked --lib
```

The automated fake-machine test uses port `18088`, separate from the manually started fixture. For Home Assistant checks and release instructions, see [docs/home-assistant.md](docs/home-assistant.md). The [architecture guide](docs/architecture.md) describes the shared engine, trust boundaries, API and releases. Rebuild icons with `./scripts/generate-icons.sh`; the normal build verifies them.

Automatic shot and profile imports go from GaggiMate to MyBrewFolio. Preserve the separate, reviewed machine-write flows for Notes, Profile Store and maintenance. Do not add telemetry, secrets, real user library fixtures, or public-network machine targets.

## License

Unless explicitly stated otherwise, contributions are accepted under GPL-3.0-or-later. By submitting a contribution, you confirm that you have the right to license it on those terms.
