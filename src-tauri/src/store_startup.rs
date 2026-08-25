// SPDX-License-Identifier: GPL-3.0-or-later

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(target_os = "windows")]
    {
        let Ok(executable) = std::env::current_exe() else {
            return;
        };
        let Some(directory) = executable.parent() else {
            return;
        };
        let _ = std::process::Command::new(directory.join("MyBrewFolioSync.exe"))
            .arg("--autostart")
            .spawn();
    }
}
