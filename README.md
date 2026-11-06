# MyBrewFolio Sync

MyBrewFolio Sync connects one or more local GaggiMate machines to your private MyBrewFolio library. It runs on your desktop, in Docker, or as a Home Assistant app.

[MyBrewFolio](https://mybrewfolio.com/info) helps you track and share brews, organize beans and grinders, discover profiles, and explore your espresso with analysis and statistics. It supports GaggiMate today, with more machine integrations planned.

## Get started

Choose the installation that works for you:

- **Desktop:** Download Sync from [MyBrewFolio Support](https://mybrewfolio.com/support/sync), install it, and open it.
- **Docker or NAS:** Follow the [Docker guide](docs/headless.md), including Unraid and Synology.
- **Home Assistant OS:** Install the [Home Assistant app](home-assistant/mybrewfolio_sync/DOCS.md). Home Assistant Container users can use Docker.

In the desktop app, choose **Connect MyBrewFolio** and approve the connection in your browser. Add a new machine or select an existing account machine and enter its local address. Then open **Account → MyBrewFolio Sync**, choose your import preference, and start the first sync. You can add more machines later.

## How Sync works

Sync automatically imports shots and profiles from GaggiMate into MyBrewFolio. It catches up after an offline period, and each machine has its own status and sync cycle. Manage the schedule, matching, conflicts and resync in **Account → MyBrewFolio Sync**. Removing a machine from an installation leaves its MyBrewFolio history intact.

Optional two-way Notes Sync requires a backup and review before machine Notes can be changed. A Profile Store installation and supported maintenance actions also require user action. Sync does not automatically delete machine content.

Your machine address stays local. Desktop credentials are stored in the operating system keychain; Docker and Home Assistant use encrypted local storage. See the [architecture and security details](docs/architecture.md) and [privacy information](https://mybrewfolio.com/legal/storage#sync-companion).

## Contributing

Contributions are welcome! See [CONTRIBUTING.md](CONTRIBUTING.md) for development and verification, and [architecture](docs/architecture.md) for the technical design. Please report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

MyBrewFolio Sync is licensed under GPL-3.0-or-later.
