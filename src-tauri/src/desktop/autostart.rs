// SPDX-License-Identifier: GPL-3.0-or-later

use super::{updates::store_managed_updates, TrayAutostartItem};
use serde::Serialize;
use tauri::Manager;
use tauri_plugin_autostart::ManagerExt as AutostartManagerExt;

#[cfg(target_os = "windows")]
pub(super) const STORE_STARTUP_TASK_ID: &str = "MyBrewFolioSyncStartup";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct AutostartStatus {
    pub(super) enabled: bool,
    pub(super) requires_windows_settings: bool,
    pub(super) blocked_by_policy: bool,
    pub(super) migration_available: bool,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StoreStartupTaskState {
    Enabled,
    Disabled,
    DisabledByUser,
    DisabledByPolicy,
}

#[cfg(any(target_os = "windows", test))]
pub(super) fn autostart_status_from_state(
    state: StoreStartupTaskState,
    legacy_enabled: bool,
) -> AutostartStatus {
    match state {
        StoreStartupTaskState::Enabled => AutostartStatus {
            enabled: true,
            requires_windows_settings: false,
            blocked_by_policy: false,
            migration_available: false,
        },
        StoreStartupTaskState::Disabled => AutostartStatus {
            enabled: false,
            requires_windows_settings: false,
            blocked_by_policy: false,
            migration_available: legacy_enabled,
        },
        StoreStartupTaskState::DisabledByUser => AutostartStatus {
            enabled: false,
            requires_windows_settings: true,
            blocked_by_policy: false,
            migration_available: false,
        },
        StoreStartupTaskState::DisabledByPolicy => AutostartStatus {
            enabled: false,
            requires_windows_settings: false,
            blocked_by_policy: true,
            migration_available: false,
        },
    }
}

pub(super) fn legacy_autostart_status(enabled: bool) -> AutostartStatus {
    AutostartStatus {
        enabled,
        requires_windows_settings: false,
        blocked_by_policy: false,
        migration_available: false,
    }
}

#[cfg(target_os = "windows")]
pub(super) fn windows_version() -> String {
    std::process::Command::new("cmd")
        .args(["/C", "ver"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Windows (version unavailable)".into())
}

#[cfg(not(target_os = "windows"))]
pub(super) fn windows_version() -> String {
    std::env::consts::OS.into()
}

#[cfg(target_os = "windows")]
pub(super) fn store_startup_task_state(
    state: windows::ApplicationModel::StartupTaskState,
) -> StoreStartupTaskState {
    use windows::ApplicationModel::StartupTaskState;

    match state {
        StartupTaskState::Enabled | StartupTaskState::EnabledByPolicy => {
            StoreStartupTaskState::Enabled
        }
        StartupTaskState::DisabledByUser => StoreStartupTaskState::DisabledByUser,
        StartupTaskState::DisabledByPolicy => StoreStartupTaskState::DisabledByPolicy,
        _ => StoreStartupTaskState::Disabled,
    }
}

#[cfg(target_os = "windows")]
pub(super) async fn store_startup_task() -> Result<windows::ApplicationModel::StartupTask, String> {
    use windows::{core::HSTRING, ApplicationModel::StartupTask};

    StartupTask::GetAsync(&HSTRING::from(STORE_STARTUP_TASK_ID))
        .map_err(|error| error.to_string())?
        .get()
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "windows")]
pub(super) async fn store_autostart_status(
    legacy_enabled: bool,
) -> Result<AutostartStatus, String> {
    let task = store_startup_task().await?;
    Ok(autostart_status_from_state(
        store_startup_task_state(task.State().map_err(|error| error.to_string())?),
        legacy_enabled,
    ))
}

#[cfg(not(target_os = "windows"))]
pub(super) async fn store_autostart_status(
    _legacy_enabled: bool,
) -> Result<AutostartStatus, String> {
    Err("Microsoft Store autostart is only available on Windows".into())
}

#[cfg(target_os = "windows")]
pub(super) async fn request_store_startup_task_enable(
    app: &tauri::AppHandle,
    task: windows::ApplicationModel::StartupTask,
) -> Result<windows::ApplicationModel::StartupTaskState, String> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = sender.send(task.RequestEnableAsync().map_err(|error| error.to_string()));
    })
    .map_err(|error| error.to_string())?;
    let operation = receiver
        .await
        .map_err(|_| "Windows could not request startup permission".to_string())??;
    operation.get().map_err(|error| error.to_string())
}

#[cfg(target_os = "windows")]
pub(super) async fn set_store_autostart(
    app: &tauri::AppHandle,
    enabled: bool,
    legacy_enabled: bool,
) -> Result<AutostartStatus, String> {
    let task = store_startup_task().await?;
    if enabled {
        let state = store_startup_task_state(task.State().map_err(|error| error.to_string())?);
        if state == StoreStartupTaskState::Disabled {
            // Windows shows its own consent UI here. A user-disabled task cannot
            // be re-enabled programmatically and is reported below instead.
            let state = request_store_startup_task_enable(app, task).await?;
            return Ok(autostart_status_from_state(
                store_startup_task_state(state),
                legacy_enabled,
            ));
        }
    } else {
        task.Disable().map_err(|error| error.to_string())?;
    }

    store_autostart_status(legacy_enabled).await
}

#[cfg(not(target_os = "windows"))]
pub(super) async fn set_store_autostart(
    _app: &tauri::AppHandle,
    _enabled: bool,
    _legacy_enabled: bool,
) -> Result<AutostartStatus, String> {
    Err("Microsoft Store autostart is only available on Windows".into())
}

pub(super) async fn autostart_status(app: &tauri::AppHandle) -> Result<AutostartStatus, String> {
    let legacy_enabled = app
        .autolaunch()
        .is_enabled()
        .map_err(|error| error.to_string())?;
    if store_managed_updates() {
        store_autostart_status(legacy_enabled).await
    } else {
        Ok(legacy_autostart_status(legacy_enabled))
    }
}

pub(super) async fn set_autostart(
    app: &tauri::AppHandle,
    enabled: bool,
) -> Result<AutostartStatus, String> {
    if store_managed_updates() {
        let legacy_enabled = app
            .autolaunch()
            .is_enabled()
            .map_err(|error| error.to_string())?;
        let status = set_store_autostart(app, enabled, legacy_enabled).await?;
        if !enabled || status.enabled {
            // Store builds previously used this registry value. Remove it once
            // the native task is authoritative so it cannot launch a second copy.
            let _ = app.autolaunch().disable();
        }
        Ok(status)
    } else {
        if enabled {
            app.autolaunch().enable()
        } else {
            app.autolaunch().disable()
        }
        .map_err(|error| error.to_string())?;
        Ok(legacy_autostart_status(enabled))
    }
}

#[tauri::command]
pub(super) async fn get_autostart_status(app: tauri::AppHandle) -> Result<AutostartStatus, String> {
    autostart_status(&app).await
}

#[tauri::command]
pub(super) async fn set_autostart_enabled(
    app: tauri::AppHandle,
    enabled: bool,
) -> Result<AutostartStatus, String> {
    set_autostart(&app, enabled).await
}

pub(super) fn update_autostart_tray_item(app: &tauri::AppHandle, status: &AutostartStatus) {
    if let Some(item) = app.try_state::<TrayAutostartItem>() {
        let text = if status.enabled {
            "Disable start with computer"
        } else if status.requires_windows_settings {
            "Enable start with computer in Windows Settings"
        } else if status.blocked_by_policy {
            "Start with computer is managed by Windows"
        } else {
            "Start with computer"
        };
        let _ = item.0.set_text(text);
        let _ = item
            .0
            .set_enabled(!status.requires_windows_settings && !status.blocked_by_policy);
    }
}
