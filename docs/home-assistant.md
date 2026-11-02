# Home Assistant distribution

The Home Assistant app is an optional web adapter over the existing `mybrewfolio-syncd` daemon,
`MachineManager` and SyncEngine. It contains no separate sync implementation. See
[the user guide](../home-assistant/mybrewfolio_sync/DOCS.md) for setup and recovery.

[![Add the MyBrewFolio app repository to Home Assistant](https://my.home-assistant.io/badges/supervisor_add_addon_repository.svg)](https://my.home-assistant.io/redirect/supervisor_add_addon_repository/?repository_url=https%3A%2F%2Fgithub.com%2Fmodsmthng%2FMyBrewFolio-Sync%23home-assistant)

The installation button becomes usable after the first catalog publication. The catalog URL is
`https://github.com/modsmthng/MyBrewFolio-Sync#home-assistant`; do not rename the branch, repository
or `mybrewfolio_sync` slug after users install it.

## Runtime boundary

Build with `--no-default-features --features headless-web` and start
`mybrewfolio-syncd daemon --home-assistant`. This adds port 8099 only in this explicit mode.
The regular desktop/headless startup behavior is unchanged. HA mode pairs with `initial_name: None`,
so even an empty account waits for an explicit machine choice. The normal Docker daemon retains
its first-machine setup behavior.

The existing pairing worker publishes a non-secret setup snapshot to the HTTP adapter. Ingress
requests may come only from the Supervisor's TCP address `172.30.32.2`; forwarded headers never
establish trust. Every mutation requires a per-process CSRF token obtained through the same-origin
status endpoint, JSON content type and an allowlisted action. No CORS is enabled, cross-site fetch
requests are rejected, static assets use relative paths and private state files cannot be served.
MyBrewFolio tokens, pending OAuth verifiers and polling credentials are never returned to the UI.

The adapter exposes local `/api/status`, `/api/machines`, and `/api/action`. Actions are limited to
pairing, machine create/connect/rename/address/remove, and account disconnect. Removal and
disconnection require explicit confirmation. Engine-specific writes and recovery stay in the
existing reviewed MyBrewFolio control channel. No public cloud API or database migration is needed.

The `home-assistant` Docker target adds web assets and a small root startup wrapper to the shared
runtime image. It creates/chowns only the dedicated `/data/state` directory, then drops to UID/GID
10001. Supervisor's options remain untouched. The standard Docker target remains the default.

## Release sequence

1. Update the existing package/Cargo/Tauri versions and
   `home-assistant/mybrewfolio_sync/config.json` together. The common Release version job checks all
   four before any release builds. This does not require manual changes on the catalog branch.
2. Push the ordinary `vX.Y.Z` tag. The existing Release workflow builds the HA image for
   `linux/amd64` and `linux/arm64`, runs the container test on each (QEMU for ARM64), and publishes
   tested per-architecture staging tags. Retries reuse an existing staging image instead of
   changing it. The manual Release dry run builds/tests but publishes no image or catalog.
3. After both tests and the desktop release jobs succeed, the workflow creates immutable
   `ghcr.io/modsmthng/mybrewfolio-sync-ha:X.Y.Z` and attaches `home-assistant-build.json` to the
   draft release. The receipt records the source SHA, multi-architecture digest and each tested
   platform digest. A rerun cannot replace an existing version with different content.
4. Review the draft and ensure the receipt exists, then publish the stable GitHub release.
   `home-assistant-publish.yml` checks the release is neither draft nor prerelease, checks the tag,
   source SHA, receipt and registry digests, and generates the `home-assistant` branch. Only
   repository metadata, user docs/artwork, app configuration and the receipt are copied there.
5. HA sees the new manifest version and can offer its normal update. The catalog never uses
   `latest`. Serialized publication, semantic version comparison and a normal non-force push
   prevent older/racing runs from downgrading it. A failed validation leaves the catalog unchanged.

If publication happened before image/receipt completion, rerun **Publish Home Assistant catalog**
with the already published stable tag after the Release workflow succeeds. No rebuild is needed.
Releases published automatically by a workflow using `GITHUB_TOKEN` do not trigger another workflow;
keep the existing manual release-publication step, or explicitly dispatch the catalog workflow.
Store `store-v*` releases and prereleases cannot publish the HA catalog.

### One-time GitHub setup

The existing repository OAuth client variable is reused, as is `GITHUB_TOKEN` for this repository's
registry and catalog branch. No cross-repository token is needed. The new GHCR package must be
**public** before user installation; GitHub may initially create it private. Allow the workflow
`contents: write` on the generated branch and `packages: write` for image publication. If branch
rules require pull requests, configure a narrowly scoped exception for the generated catalog branch.
Do not add a client secret or private signing key to the image or catalog.

## Verification

```sh
npm run build:ha
npm run test:ha
npm run test:ha-release
cargo test --locked --manifest-path src-tauri/Cargo.toml --no-default-features --features headless-web
docker build --target home-assistant -t mybrewfolio-sync-ha:test .
node tests/verify-ha-container.mjs mybrewfolio-sync-ha:test
```

The container test uses its own disposable internal `172.30.32.0/24` network, volumes and containers.
Run it away from a real Supervisor using that subnet. It tests the real TCP-peer boundary, relative
assets, CSRF, non-root daemon, unchanged options, persistent key and cold-backup restoration.

Before public distribution, test on real HAOS: install from the catalog; verify an administrator
can use Ingress and a non-administrator cannot; authorize a new and an existing account; connect
multiple machines; complete the first import; interrupt/reconnect LAN and internet; restart/update
and restore a cold backup. Include ARM64 hardware and check the installed image architecture.
The simulated Supervisor test is not a substitute for that final hardware acceptance.
