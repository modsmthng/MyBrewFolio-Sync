// SPDX-License-Identifier: GPL-3.0-or-later

fn main() {
    println!("cargo:rerun-if-env-changed=MYBREWFOLIO_SYNC_WINDOWS_STORE_BUILD");
    #[cfg(feature = "desktop")]
    tauri_build::build()
}
