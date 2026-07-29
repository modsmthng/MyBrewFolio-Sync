// SPDX-License-Identifier: GPL-3.0-or-later

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    mybrewfolio_sync_lib::run();
}
