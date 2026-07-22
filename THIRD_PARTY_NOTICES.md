# MyBrewFolio Sync Third-Party Notices

Last updated: 22 July 2026

MyBrewFolio Sync is built with open-source software. Exact versions and the
authoritative licence expression for every dependency are recorded in
`package-lock.json` and `src-tauri/Cargo.lock` in the source release.

## Principal components

- **Tauri 2 and official Tauri plugins**: MIT or Apache-2.0
- **Preact**: MIT
- **Vite and Rollup**: MIT
- **Tokio, Reqwest, Serde, Chrono and supporting Rust crates**: primarily MIT or Apache-2.0
- **Rusqlite and libsqlite3-sys**: MIT; the bundled SQLite library is in the public domain
- **Keyring**: MIT or Apache-2.0
- **rustls and Ring**: Apache-2.0, ISC and MIT as declared by the packages
- **WebView rendering dependencies**: their platform and package licences, including MIT, Apache-2.0, BSD and MPL-2.0

Transitive dependencies additionally use permissive licences including ISC,
BSD-2-Clause, BSD-3-Clause, Zlib, Unicode-3.0, CC0-1.0, CC-BY-4.0 and the
Unlicense. Copyright and licence files supplied by each dependency remain
authoritative.

Source code and complete dependency metadata are available at:

https://github.com/modsmthng/MyBrewFolio-Sync

The MyBrewFolio Sync licence does not replace or restrict the licences of
the components listed above.
