// SPDX-License-Identifier: GPL-3.0-or-later

use super::{
    show_main_window, RestartSchedule, UpdateRestartState, UpdateStatus, UPDATE_AVAILABLE_VERSION,
    UPDATE_CHECK_FAILED_MESSAGE, UPDATE_CHECK_INTERVAL_HOURS, UPDATE_INSTALL_FAILED_MESSAGE,
    UPDATE_LAST_CHECK_AT, UPDATE_LAST_PROMPT_AT, UPDATE_PROMPT_PENDING, UPDATE_RESTART_VERSION,
};
use crate::{engine::SyncEngine, store::AppStore};
use std::sync::{atomic::Ordering, Arc};
use tauri::{Emitter, State};
use tauri_plugin_updater::UpdaterExt;

pub(super) fn stored_update_status(
    store: &AppStore,
    restart_state: &UpdateRestartState,
) -> Result<UpdateStatus, String> {
    if store_managed_updates() {
        return Ok(UpdateStatus::StoreManaged);
    }
    let public_key = option_env!("MYBREWFOLIO_SYNC_UPDATER_PUBLIC_KEY")
        .unwrap_or("")
        .trim();
    if public_key.is_empty() {
        return Ok(UpdateStatus::NotConfigured);
    }
    if let Some(version) = store
        .setting(UPDATE_RESTART_VERSION)
        .map_err(|error| error.to_string())?
    {
        return Ok(UpdateStatus::Installed {
            version,
            restart_requested: restart_state.requested.load(Ordering::SeqCst),
            restart_waiting_for_sync: false,
        });
    }
    if let Some(version) = store
        .setting(UPDATE_AVAILABLE_VERSION)
        .map_err(|error| error.to_string())?
    {
        let prompt_pending = store
            .setting(UPDATE_PROMPT_PENDING)
            .map_err(|error| error.to_string())?
            .as_deref()
            == Some("1");
        return Ok(UpdateStatus::Available {
            version,
            prompt_pending,
        });
    }
    Ok(UpdateStatus::Unknown)
}

pub(super) fn update_due(last_check: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> bool {
    let Some(last_check) = last_check else {
        return true;
    };
    chrono::DateTime::parse_from_rfc3339(last_check)
        .map(|last| {
            now - last.with_timezone(&chrono::Utc)
                >= chrono::Duration::hours(UPDATE_CHECK_INTERVAL_HOURS)
        })
        .unwrap_or(true)
}

pub(super) fn update_check_required(
    last_check: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
    force: bool,
) -> bool {
    force || update_due(last_check, now)
}

pub(super) fn update_prompt_due(
    last_prompt: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    update_due(last_prompt, now)
}

pub(super) fn restart_schedule(syncing: bool) -> RestartSchedule {
    if syncing {
        RestartSchedule::WaitForSync
    } else {
        RestartSchedule::Now
    }
}

pub(super) async fn emit_update_status(app: &tauri::AppHandle, status: &UpdateStatus) {
    let _ = app.emit("update-status-changed", status);
}

pub(super) async fn check_for_update(
    app: &tauri::AppHandle,
    store: &AppStore,
    restart_state: &UpdateRestartState,
    force: bool,
) -> Result<UpdateStatus, String> {
    let current = stored_update_status(store, restart_state)?;
    if matches!(
        current,
        UpdateStatus::StoreManaged | UpdateStatus::NotConfigured | UpdateStatus::Installed { .. }
    ) {
        return Ok(current);
    }
    let now = chrono::Utc::now();
    let last_check = store
        .setting(UPDATE_LAST_CHECK_AT)
        .map_err(|error| error.to_string())?;
    if !update_check_required(last_check.as_deref(), now, force) {
        return Ok(current);
    }

    store
        .set_setting(UPDATE_LAST_CHECK_AT, &now.to_rfc3339())
        .map_err(|error| error.to_string())?;
    let updater = app.updater().map_err(|error| error.to_string())?;
    let checked = updater.check().await.map_err(|error| error.to_string())?;
    let status = if let Some(update) = checked {
        let version = update.version.to_string();
        let previous_version = store
            .setting(UPDATE_AVAILABLE_VERSION)
            .map_err(|error| error.to_string())?;
        let last_prompt = store
            .setting(UPDATE_LAST_PROMPT_AT)
            .map_err(|error| error.to_string())?;
        let prompt_pending = force
            || previous_version.as_deref() != Some(version.as_str())
            || update_prompt_due(last_prompt.as_deref(), now);
        store
            .set_setting(UPDATE_AVAILABLE_VERSION, &version)
            .map_err(|error| error.to_string())?;
        if prompt_pending {
            store
                .set_setting(UPDATE_LAST_PROMPT_AT, &now.to_rfc3339())
                .map_err(|error| error.to_string())?;
            store
                .set_setting(UPDATE_PROMPT_PENDING, "1")
                .map_err(|error| error.to_string())?;
        }
        UpdateStatus::Available {
            version,
            prompt_pending,
        }
    } else {
        store
            .remove_setting(UPDATE_AVAILABLE_VERSION)
            .map_err(|error| error.to_string())?;
        store
            .remove_setting(UPDATE_PROMPT_PENDING)
            .map_err(|error| error.to_string())?;
        UpdateStatus::UpToDate
    };
    if matches!(
        status,
        UpdateStatus::Available {
            prompt_pending: true,
            ..
        }
    ) {
        show_main_window(app);
    }
    emit_update_status(app, &status).await;
    Ok(status)
}

pub(super) async fn run_update_check(
    app: &tauri::AppHandle,
    store: &AppStore,
    restart_state: &UpdateRestartState,
    force: bool,
) -> UpdateStatus {
    match check_for_update(app, store, restart_state, force).await {
        Ok(status) => status,
        Err(_) => {
            let status = UpdateStatus::Error {
                message: UPDATE_CHECK_FAILED_MESSAGE.into(),
            };
            emit_update_status(app, &status).await;
            status
        }
    }
}

#[tauri::command]
pub(super) fn get_update_status(
    store: State<'_, Arc<AppStore>>,
    restart_state: State<'_, Arc<UpdateRestartState>>,
) -> Result<UpdateStatus, String> {
    stored_update_status(&store, &restart_state)
}

#[tauri::command]
pub(super) async fn check_update(
    app: tauri::AppHandle,
    store: State<'_, Arc<AppStore>>,
    restart_state: State<'_, Arc<UpdateRestartState>>,
) -> Result<UpdateStatus, String> {
    Ok(run_update_check(&app, &store, &restart_state, true).await)
}

#[tauri::command]
pub(super) async fn dismiss_update(
    app: tauri::AppHandle,
    store: State<'_, Arc<AppStore>>,
    restart_state: State<'_, Arc<UpdateRestartState>>,
) -> Result<UpdateStatus, String> {
    store
        .set_setting(UPDATE_PROMPT_PENDING, "0")
        .map_err(|error| error.to_string())?;
    let status = stored_update_status(&store, &restart_state)?;
    emit_update_status(&app, &status).await;
    Ok(status)
}

#[tauri::command]
pub(super) async fn install_update(
    app: tauri::AppHandle,
    store: State<'_, Arc<AppStore>>,
    restart_state: State<'_, Arc<UpdateRestartState>>,
) -> Result<UpdateStatus, String> {
    match install_available_update(&app, &store, &restart_state).await {
        Ok(status) => Ok(status),
        Err(_) => {
            let status = UpdateStatus::Error {
                message: UPDATE_INSTALL_FAILED_MESSAGE.into(),
            };
            emit_update_status(&app, &status).await;
            Ok(status)
        }
    }
}

pub(super) async fn install_available_update(
    app: &tauri::AppHandle,
    store: &AppStore,
    restart_state: &UpdateRestartState,
) -> Result<UpdateStatus, String> {
    let current = stored_update_status(store, restart_state)?;
    if matches!(
        current,
        UpdateStatus::StoreManaged | UpdateStatus::NotConfigured | UpdateStatus::Installed { .. }
    ) {
        return Ok(current);
    }
    let updater = app.updater().map_err(|error| error.to_string())?;
    let Some(update) = updater.check().await.map_err(|error| error.to_string())? else {
        store
            .remove_setting(UPDATE_AVAILABLE_VERSION)
            .map_err(|error| error.to_string())?;
        store
            .remove_setting(UPDATE_PROMPT_PENDING)
            .map_err(|error| error.to_string())?;
        let status = UpdateStatus::UpToDate;
        emit_update_status(app, &status).await;
        return Ok(status);
    };
    let version = update.version.to_string();
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|error| error.to_string())?;
    store
        .set_setting(UPDATE_RESTART_VERSION, &version)
        .map_err(|error| error.to_string())?;
    store
        .remove_setting(UPDATE_AVAILABLE_VERSION)
        .map_err(|error| error.to_string())?;
    store
        .remove_setting(UPDATE_PROMPT_PENDING)
        .map_err(|error| error.to_string())?;
    restart_state.requested.store(false, Ordering::SeqCst);
    let status = UpdateStatus::Installed {
        version,
        restart_requested: false,
        restart_waiting_for_sync: false,
    };
    emit_update_status(app, &status).await;
    Ok(status)
}

#[tauri::command]
pub(super) async fn restart_after_update(
    app: tauri::AppHandle,
    engine: State<'_, Arc<SyncEngine>>,
    store: State<'_, Arc<AppStore>>,
    restart_state: State<'_, Arc<UpdateRestartState>>,
) -> Result<UpdateStatus, String> {
    let UpdateStatus::Installed { version, .. } = stored_update_status(&store, &restart_state)?
    else {
        return Err("No installed update is waiting for a restart".into());
    };
    let schedule = restart_schedule(engine.status().await.syncing);
    restart_state.requested.store(true, Ordering::SeqCst);
    let status = UpdateStatus::Installed {
        version,
        restart_requested: true,
        restart_waiting_for_sync: schedule == RestartSchedule::WaitForSync,
    };
    emit_update_status(&app, &status).await;
    let restart_engine = engine.inner().clone();
    let restart_handle = app.clone();
    let restart_store = store.inner().clone();
    tauri::async_runtime::spawn(async move {
        let _pause = restart_engine.pause_operations().await;
        let _ = restart_store.remove_setting(UPDATE_RESTART_VERSION);
        restart_handle.request_restart();
        // Keep the engine paused until the application actually exits.
        std::future::pending::<()>().await;
    });
    Ok(status)
}

pub(super) fn is_store_managed_build(target_is_windows: bool, store_build: Option<&str>) -> bool {
    target_is_windows && matches!(store_build, Some("true"))
}

pub(super) fn store_managed_updates() -> bool {
    is_store_managed_build(
        cfg!(target_os = "windows"),
        option_env!("MYBREWFOLIO_SYNC_WINDOWS_STORE_BUILD"),
    )
}
