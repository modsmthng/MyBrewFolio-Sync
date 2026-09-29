// SPDX-License-Identifier: GPL-3.0-or-later

mod autostart;
mod runtime;
#[cfg(test)]
mod tests;
mod updates;

use autostart::{autostart_status, set_autostart, update_autostart_tray_item, windows_version};
#[cfg(test)]
use autostart::{autostart_status_from_state, StoreStartupTaskState};
use runtime::start_background_services;
use updates::store_managed_updates;
#[cfg(test)]
use updates::{is_store_managed_build, restart_schedule, update_check_required, update_due};

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use crate::{
    credentials::KeyringCredentialStore, engine::SyncEngine, machines::MachineManager,
    model::AppStatus, store::AppStore,
};
use serde::Serialize;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager, State,
};
#[cfg(target_os = "macos")]
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_autostart::ManagerExt as AutostartManagerExt;
use tauri_plugin_opener::OpenerExt;

pub(crate) struct TrayStatusItem(MenuItem<tauri::Wry>);
pub(crate) struct TrayMachineItem(MenuItem<tauri::Wry>);
pub(crate) struct TrayErrorItem(MenuItem<tauri::Wry>);
pub(crate) struct TrayAutostartItem(MenuItem<tauri::Wry>);

const UPDATE_CHECK_INTERVAL_HOURS: i64 = 24;
const UPDATE_LAST_CHECK_AT: &str = "update_last_check_at";
const UPDATE_AVAILABLE_VERSION: &str = "update_available_version";
const UPDATE_LAST_PROMPT_AT: &str = "update_last_prompt_at";
const UPDATE_PROMPT_PENDING: &str = "update_prompt_pending";
const UPDATE_RESTART_VERSION: &str = "update_restart_version";
const UPDATE_CHECK_FAILED_MESSAGE: &str = "Unable to check for updates. Sync will try again later.";
const UPDATE_INSTALL_FAILED_MESSAGE: &str = "Unable to install the update. Please try again later.";

#[derive(Default)]
struct UpdateRestartState {
    requested: AtomicBool,
}

#[derive(Default)]
struct ReconnectPromptState {
    shown: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartSchedule {
    Now,
    WaitForSync,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum UpdateStatus {
    Unknown,
    UpToDate,
    Available {
        version: String,
        prompt_pending: bool,
    },
    Installed {
        version: String,
        restart_requested: bool,
        restart_waiting_for_sync: bool,
    },
    Error {
        message: String,
    },
    StoreManaged,
    NotConfigured,
}

fn should_show_reconnect_prompt(status: &AppStatus, shown: &AtomicBool) -> bool {
    if status.connected {
        shown.store(false, Ordering::SeqCst);
        return false;
    }
    matches!(
        status.last_error_code.as_deref(),
        Some("SYNC_REAUTH_REQUIRED" | "SYNC_DEVICE_REVOKED")
    ) && !shown.swap(true, Ordering::SeqCst)
}

async fn emit_status(app: &tauri::AppHandle, engine: &SyncEngine) {
    let snapshot = if let Some(manager) = app.try_state::<Arc<MachineManager>>() {
        let _ = manager.reconcile_auth_loss().await;
        manager.status_json().await
    } else {
        serde_json::to_value(engine.status().await).unwrap_or_default()
    };
    let status: AppStatus =
        serde_json::from_value(snapshot.clone()).unwrap_or(engine.status().await);
    if let Some(prompt) = app.try_state::<ReconnectPromptState>() {
        if should_show_reconnect_prompt(&status, &prompt.shown) {
            show_main_window(app);
        }
    }
    if let Some(item) = app.try_state::<TrayStatusItem>() {
        let text = if status.syncing {
            "Syncing…"
        } else if status.last_error.is_some() {
            "Items not synchronized"
        } else if status.connected {
            "MyBrewFolio connected"
        } else {
            "MyBrewFolio not connected"
        };
        let _ = item.0.set_text(text);
    }
    if let Some(item) = app.try_state::<TrayMachineItem>() {
        let _ = item.0.set_text(format!("Machine: {}", status.machine_host));
    }
    if let Some(item) = app.try_state::<TrayErrorItem>() {
        let text = status
            .last_error
            .as_deref()
            .map(|error| format!("Last error: {}", error.chars().take(90).collect::<String>()))
            .unwrap_or_else(|| "No Sync errors".to_string());
        let _ = item.0.set_text(text);
    }
    let _ = app.emit("sync-status-changed", snapshot);
}

struct StartupDiagnostics {
    path: PathBuf,
    frontend_ready: AtomicBool,
}

impl StartupDiagnostics {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            frontend_ready: AtomicBool::new(false),
        }
    }

    fn reset(&self, app_version: &str) {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let webview_version =
            tauri::webview_version().unwrap_or_else(|error| format!("unavailable ({error})"));
        let content = format!(
        "MyBrewFolio Sync startup diagnostics\nstarted_utc={}\napp_version={}\nos={}\nwebview2_version={}\ntauri_started=true\nfrontend_ready=false\n",
        chrono::Utc::now().to_rfc3339(),
        app_version,
        windows_version(),
        webview_version,
    );
        let _ = fs::write(&self.path, content);
    }

    fn append(&self, line: &str) {
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "{line}");
        }
    }

    fn mark_frontend_ready(&self) {
        if !self.frontend_ready.swap(true, Ordering::SeqCst) {
            self.append(&format!(
                "frontend_ready=true\nfrontend_ready_utc={}",
                chrono::Utc::now().to_rfc3339()
            ));
        }
    }
}

#[tauri::command]
fn frontend_ready(diagnostics: State<'_, Arc<StartupDiagnostics>>) {
    diagnostics.mark_frontend_ready();
}

#[tauri::command]
async fn get_status(manager: State<'_, Arc<MachineManager>>) -> Result<serde_json::Value, String> {
    Ok(manager.status_json().await)
}

#[tauri::command]
async fn set_machine_host(
    app: tauri::AppHandle,
    manager: State<'_, Arc<MachineManager>>,
    host: String,
    machine_id: Option<String>,
) -> Result<(), String> {
    let account = manager.account_operation().await;
    let engine = manager.selected_engine(machine_id.as_deref()).await?;
    engine
        .set_host(&host)
        .await
        .map_err(|error| error.to_string())?;
    drop(account);
    emit_status(&app, &manager.primary()).await;
    Ok(())
}

#[tauri::command]
async fn list_account_machines(
    manager: State<'_, Arc<MachineManager>>,
) -> Result<Vec<serde_json::Value>, String> {
    manager.list_account_machines().await
}

#[tauri::command]
async fn add_machine(
    app: tauri::AppHandle,
    manager: State<'_, Arc<MachineManager>>,
    name: String,
    host: String,
) -> Result<String, String> {
    let machine_id = manager.add_machine(&name, &host).await?;
    emit_status(&app, &manager.primary()).await;
    Ok(machine_id)
}

#[tauri::command]
async fn connect_machine(
    app: tauri::AppHandle,
    manager: State<'_, Arc<MachineManager>>,
    machine_id: String,
    host: String,
) -> Result<(), String> {
    manager.connect_machine(&machine_id, &host).await?;
    emit_status(&app, &manager.primary()).await;
    Ok(())
}

#[tauri::command]
async fn rename_machine(
    app: tauri::AppHandle,
    manager: State<'_, Arc<MachineManager>>,
    machine_id: String,
    name: String,
) -> Result<(), String> {
    manager.rename_machine(&machine_id, &name).await?;
    emit_status(&app, &manager.primary()).await;
    Ok(())
}

#[tauri::command]
async fn remove_machine(
    app: tauri::AppHandle,
    manager: State<'_, Arc<MachineManager>>,
    machine_id: String,
) -> Result<(), String> {
    manager.remove_machine(&machine_id).await?;
    emit_status(&app, &manager.primary()).await;
    Ok(())
}

fn apply_app_icon_visibility(app: &tauri::AppHandle, hidden: bool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let policy = if hidden {
            tauri::ActivationPolicy::Accessory
        } else {
            tauri::ActivationPolicy::Regular
        };
        app.set_activation_policy(policy)
            .map_err(|error| error.to_string())?;
        app.set_dock_visibility(!hidden)
            .map_err(|error| error.to_string())?;
    }

    #[cfg(not(target_os = "macos"))]
    if let Some(window) = app.get_webview_window("main") {
        window
            .set_skip_taskbar(hidden)
            .map_err(|error| error.to_string())?;
    }

    Ok(())
}

#[tauri::command]
fn get_hide_app_icon(engine: State<'_, Arc<SyncEngine>>) -> Result<bool, String> {
    engine.hide_app_icon().map_err(|error| error.to_string())
}

#[tauri::command]
fn set_hide_app_icon(
    app: tauri::AppHandle,
    engine: State<'_, Arc<SyncEngine>>,
    hidden: bool,
) -> Result<(), String> {
    let previous = engine.hide_app_icon().map_err(|error| error.to_string())?;
    engine
        .set_hide_app_icon(hidden)
        .map_err(|error| error.to_string())?;
    if let Err(error) = apply_app_icon_visibility(&app, hidden) {
        let _ = engine.set_hide_app_icon(previous);
        return Err(error);
    }
    Ok(())
}

#[tauri::command]
async fn begin_oauth(
    app: tauri::AppHandle,
    engine: State<'_, Arc<SyncEngine>>,
    manager: State<'_, Arc<MachineManager>>,
    machine_name: Option<String>,
) -> Result<(), String> {
    if let Some(machine_name) = machine_name {
        let name = MachineManager::validate_name(&machine_name)?;
        manager
            .registry()
            .set_setting("pending_machine_name", &name)
            .map_err(|error| error.to_string())?;
    } else {
        manager
            .registry()
            .remove_setting("pending_machine_name")
            .map_err(|error| error.to_string())?;
    }
    let url = engine
        .begin_oauth()
        .await
        .map_err(|error| error.to_string())?;
    app.opener()
        .open_url(url.as_str(), None::<&str>)
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn complete_oauth(
    app: tauri::AppHandle,
    engine: State<'_, Arc<SyncEngine>>,
    manager: State<'_, Arc<MachineManager>>,
    callback_url: String,
) -> Result<(), String> {
    let initial_name = manager
        .registry()
        .setting("pending_machine_name")
        .map_err(|error| error.to_string())?;
    manager
        .complete_oauth_and_authorize(&callback_url, initial_name)
        .await?;
    manager
        .registry()
        .remove_setting("pending_machine_name")
        .map_err(|error| error.to_string())?;
    // Direct installs retain their existing onboarding behavior. Store builds
    // must ask Windows for explicit startup-task consent from the setting.
    if !store_managed_updates() {
        let _ = app.autolaunch().enable();
    }
    emit_status(&app, &engine).await;
    let manager = manager.inner().clone();
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = manager.sync_all().await;
        emit_status(&handle, &manager.primary()).await;
    });
    Ok(())
}

#[tauri::command]
async fn sync_now(
    app: tauri::AppHandle,
    manager: State<'_, Arc<MachineManager>>,
    machine_id: Option<String>,
) -> Result<(), String> {
    let result = match machine_id.as_deref() {
        Some(machine_id) => manager.sync_machine(machine_id).await,
        None => manager.sync_all_checked().await,
    };
    emit_status(&app, &manager.primary()).await;
    result
}

#[tauri::command]
async fn configure_sync(
    app: tauri::AppHandle,
    manager: State<'_, Arc<MachineManager>>,
    reuse_matching: bool,
    machine_id: Option<String>,
) -> Result<(), String> {
    let account = manager.account_operation().await;
    let engine = manager.selected_engine(machine_id.as_deref()).await?;
    engine
        .configure_sync(reuse_matching)
        .await
        .map_err(|error| error.to_string())?;
    drop(account);
    let result = manager.sync_selected(machine_id.as_deref()).await;
    emit_status(&app, &engine).await;
    result
}

#[tauri::command]
async fn retry_failed_items(
    app: tauri::AppHandle,
    manager: State<'_, Arc<MachineManager>>,
    machine_id: Option<String>,
) -> Result<(), String> {
    let account = manager.account_operation().await;
    let engine = manager.selected_engine(machine_id.as_deref()).await?;
    engine
        .retry_failures()
        .await
        .map_err(|error| error.to_string())?;
    drop(account);
    let result = manager.sync_selected(machine_id.as_deref()).await;
    emit_status(&app, &engine).await;
    result
}

#[tauri::command]
async fn dismiss_notes_sync_intro(
    app: tauri::AppHandle,
    manager: State<'_, Arc<MachineManager>>,
    machine_id: Option<String>,
) -> Result<(), String> {
    let account = manager.account_operation().await;
    let engine = manager.selected_engine(machine_id.as_deref()).await?;
    engine
        .dismiss_notes_sync_intro()
        .await
        .map_err(|error| error.to_string())?;
    drop(account);
    emit_status(&app, &engine).await;
    Ok(())
}

#[tauri::command]
async fn begin_two_way_notes_activation(
    app: tauri::AppHandle,
    manager: State<'_, Arc<MachineManager>>,
    machine_id: Option<String>,
) -> Result<serde_json::Value, String> {
    let account = manager.account_operation().await;
    let engine = manager.selected_engine(machine_id.as_deref()).await?;
    let result = engine
        .begin_two_way_notes_activation()
        .await
        .map_err(|error| error.to_string())?;
    drop(account);
    emit_status(&app, &engine).await;
    Ok(result)
}

#[tauri::command]
async fn activate_two_way_notes(
    app: tauri::AppHandle,
    manager: State<'_, Arc<MachineManager>>,
    backup_id: String,
    decisions: serde_json::Value,
    machine_id: Option<String>,
) -> Result<serde_json::Value, String> {
    let account = manager.account_operation().await;
    let engine = manager.selected_engine(machine_id.as_deref()).await?;
    let result = engine
        .activate_two_way_notes(&backup_id, decisions)
        .await
        .map_err(|error| error.to_string())?;
    drop(account);
    emit_status(&app, &engine).await;
    Ok(result)
}

#[tauri::command]
async fn disable_two_way_notes(
    app: tauri::AppHandle,
    manager: State<'_, Arc<MachineManager>>,
    machine_id: Option<String>,
) -> Result<(), String> {
    let account = manager.account_operation().await;
    let engine = manager.selected_engine(machine_id.as_deref()).await?;
    engine
        .disable_two_way_notes()
        .await
        .map_err(|error| error.to_string())?;
    drop(account);
    emit_status(&app, &engine).await;
    Ok(())
}

#[tauri::command]
async fn create_latest_notes_backup(
    app: tauri::AppHandle,
    manager: State<'_, Arc<MachineManager>>,
    machine_id: Option<String>,
) -> Result<String, String> {
    let account = manager.account_operation().await;
    let engine = manager.selected_engine(machine_id.as_deref()).await?;
    let result = engine
        .create_latest_notes_backup()
        .await
        .map_err(|error| error.to_string())?;
    drop(account);
    emit_status(&app, &engine).await;
    Ok(result)
}

#[tauri::command]
async fn preview_notes_restore(
    manager: State<'_, Arc<MachineManager>>,
    backup_id: String,
    machine_id: Option<String>,
) -> Result<serde_json::Value, String> {
    let _account = manager.account_operation().await;
    let engine = manager.selected_engine(machine_id.as_deref()).await?;
    engine
        .preview_notes_restore(&backup_id)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn restore_notes_backup(
    app: tauri::AppHandle,
    manager: State<'_, Arc<MachineManager>>,
    backup_id: String,
    source_keys: Vec<String>,
    machine_id: Option<String>,
) -> Result<serde_json::Value, String> {
    let account = manager.account_operation().await;
    let engine = manager.selected_engine(machine_id.as_deref()).await?;
    let result = engine
        .restore_notes_backup(&backup_id, &source_keys)
        .await
        .map_err(|error| error.to_string())?;
    drop(account);
    emit_status(&app, &engine).await;
    Ok(result)
}

#[tauri::command]
async fn preview_complete_resync(
    manager: State<'_, Arc<MachineManager>>,
    machine_id: Option<String>,
) -> Result<serde_json::Value, String> {
    let _account = manager.account_operation().await;
    let engine = manager.selected_engine(machine_id.as_deref()).await?;
    engine
        .resync_preview()
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn apply_complete_resync(
    app: tauri::AppHandle,
    manager: State<'_, Arc<MachineManager>>,
    decisions: serde_json::Value,
    machine_id: Option<String>,
) -> Result<serde_json::Value, String> {
    let account = manager.account_operation().await;
    let engine = manager.selected_engine(machine_id.as_deref()).await?;
    let applied = engine
        .apply_resync(decisions)
        .await
        .map_err(|error| error.to_string())?;
    drop(account);
    let follow_up_error = manager
        .sync_selected(machine_id.as_deref())
        .await
        .err()
        .map(|error| error.to_string());
    emit_status(&app, &engine).await;
    let mut result = applied;
    if let (Some(object), Some(error)) = (result.as_object_mut(), follow_up_error) {
        object.insert("followUpError".into(), serde_json::Value::String(error));
    }
    Ok(result)
}

#[tauri::command]
async fn disconnect_account(
    app: tauri::AppHandle,
    manager: State<'_, Arc<MachineManager>>,
) -> Result<serde_json::Value, String> {
    let result = manager.disconnect_all().await?;
    emit_status(&app, &manager.primary()).await;
    Ok(result)
}

#[tauri::command]
fn open_mybrewfolio_page(
    app: tauri::AppHandle,
    page: String,
    machine_id: Option<String>,
) -> Result<(), String> {
    let url = match page.as_str() {
        "syncHelp" => "https://mybrewfolio.com/support/sync",
        "privacy" => "https://mybrewfolio.com/legal/privacy",
        "accountSync" => "https://mybrewfolio.com/account/sync",
        _ => return Err("Unknown MyBrewFolio page".into()),
    };
    let url = if page == "accountSync" {
        if let Some(machine_id) = machine_id {
            let id =
                uuid::Uuid::parse_str(&machine_id).map_err(|_| "Invalid machine ID".to_string())?;
            format!("{url}?machineId={id}")
        } else {
            url.to_owned()
        }
    } else {
        url.to_owned()
    };
    app.opener()
        .open_url(&url, None::<&str>)
        .map_err(|error| error.to_string())
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn has_autostart_argument(arguments: impl IntoIterator<Item = String>) -> bool {
    arguments
        .into_iter()
        .any(|argument| argument == "--autostart")
}

fn launched_from_autostart() -> bool {
    has_autostart_argument(std::env::args().skip(1))
}

type DesktopSetupResult<T> = Result<T, Box<dyn std::error::Error>>;

fn initialize_application(
    app: &mut tauri::App,
) -> DesktopSetupResult<(Arc<AppStore>, Arc<SyncEngine>, Arc<StartupDiagnostics>)> {
    let mut data_dir = app.path().app_data_dir()?;
    if cfg!(debug_assertions) {
        data_dir.push("development");
    }
    std::fs::create_dir_all(&data_dir)?;
    let startup_diagnostics = Arc::new(StartupDiagnostics::new(
        data_dir.join("startup-diagnostics.log"),
    ));
    startup_diagnostics.reset(&app.package_info().version.to_string());
    app.manage(startup_diagnostics.clone());
    let store = Arc::new(AppStore::open(&data_dir.join("sync.sqlite"))?);
    app.manage(store.clone());
    app.manage(Arc::new(UpdateRestartState::default()));
    app.manage(ReconnectPromptState::default());
    let manager = Arc::new(MachineManager::open(
        &data_dir,
        store.clone(),
        Arc::new(KeyringCredentialStore),
    )?);
    let engine = manager.primary();
    app.manage(manager);
    app.manage(engine.clone());
    apply_app_icon_visibility(app.handle(), engine.hide_app_icon().unwrap_or(false))
        .map_err(std::io::Error::other)?;
    Ok((store, engine, startup_diagnostics))
}

fn configure_deep_links(_app: &mut tauri::App) -> DesktopSetupResult<()> {
    #[cfg(any(target_os = "linux", all(debug_assertions, windows)))]
    {
        use tauri_plugin_deep_link::DeepLinkExt;
        _app.deep_link().register_all()?;
    }
    Ok(())
}

fn configure_tray(app: &mut tauri::App) -> DesktopSetupResult<()> {
    let status_item = MenuItem::with_id(app, "status", "MyBrewFolio Sync", false, None::<&str>)?;
    app.manage(TrayStatusItem(status_item.clone()));
    let machine_host = app
        .state::<Arc<AppStore>>()
        .setting("machine_host")?
        .unwrap_or_else(|| "gaggimate.local".to_string());
    let machine_item = MenuItem::with_id(
        app,
        "machine",
        format!("Machine: {machine_host}"),
        false,
        None::<&str>,
    )?;
    app.manage(TrayMachineItem(machine_item.clone()));
    let error_item = MenuItem::with_id(app, "error", "No Sync errors", false, None::<&str>)?;
    app.manage(TrayErrorItem(error_item.clone()));
    let show_item = MenuItem::with_id(app, "show", "Open Sync", true, None::<&str>)?;
    let sync_item = MenuItem::with_id(app, "sync", "Sync now", true, None::<&str>)?;
    let autostart_item =
        MenuItem::with_id(app, "autostart", "Start with computer", true, None::<&str>)?;
    app.manage(TrayAutostartItem(autostart_item.clone()));
    let disconnect_item =
        MenuItem::with_id(app, "disconnect", "Disconnect account", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &status_item,
            &machine_item,
            &error_item,
            &show_item,
            &sync_item,
            &autostart_item,
            &disconnect_item,
            &quit_item,
        ],
    )?;
    #[cfg(target_os = "macos")]
    let tray_icon =
        tauri::image::Image::from_bytes(include_bytes!("../../icons/tray-template.png"))?;
    #[cfg(not(target_os = "macos"))]
    let tray_icon = tauri::image::Image::from_bytes(include_bytes!("../../icons/tray-color.png"))?;
    TrayIconBuilder::new()
        .icon(tray_icon)
        .icon_as_template(cfg!(target_os = "macos"))
        .tooltip("MyBrewFolio Sync")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            "sync" => {
                let manager = app.state::<Arc<MachineManager>>().inner().clone();
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = manager.sync_all().await;
                    emit_status(&handle, &manager.primary()).await;
                });
            }
            "autostart" => {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    let Ok(current) = autostart_status(&handle).await else {
                        return;
                    };
                    if let Ok(updated) = set_autostart(&handle, !current.enabled).await {
                        update_autostart_tray_item(&handle, &updated);
                    }
                });
            }
            "disconnect" => {
                show_main_window(app);
                let _ = app.emit("disconnect-confirmation-requested", ());
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

fn configure_main_window(app: &tauri::App) {
    if let Some(window) = app.get_webview_window("main") {
        let window_handle = app.handle().clone();
        window.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if let Some(window) = window_handle.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
        });
    }
}

fn setup_application(app: &mut tauri::App, launch_in_background: bool) -> DesktopSetupResult<()> {
    let (store, _engine, startup_diagnostics) = initialize_application(app)?;
    configure_deep_links(app)?;
    configure_tray(app)?;
    if !launch_in_background {
        show_main_window(app.handle());
    }
    let manager = app.state::<Arc<MachineManager>>().inner().clone();
    start_background_services(app, manager, store, startup_diagnostics);
    configure_main_window(app);
    Ok(())
}

fn build_application(launch_in_background: bool) -> tauri::App {
    let mut builder = tauri::Builder::default();
    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }));
    }
    let mut updater = tauri_plugin_updater::Builder::new();
    if let Some(public_key) =
        option_env!("MYBREWFOLIO_SYNC_UPDATER_PUBLIC_KEY").filter(|value| !value.trim().is_empty())
    {
        updater = updater.pubkey(public_key);
    }
    let autostart = {
        let builder = tauri_plugin_autostart::Builder::new().arg("--autostart");
        #[cfg(target_os = "macos")]
        let builder = builder.macos_launcher(MacosLauncher::LaunchAgent);
        builder.build()
    };
    builder
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(updater.build())
        .plugin(autostart)
        .setup(move |app| setup_application(app, launch_in_background))
        .invoke_handler(tauri::generate_handler![
            frontend_ready,
            get_status,
            set_machine_host,
            list_account_machines,
            add_machine,
            connect_machine,
            rename_machine,
            remove_machine,
            get_hide_app_icon,
            set_hide_app_icon,
            autostart::get_autostart_status,
            autostart::set_autostart_enabled,
            begin_oauth,
            complete_oauth,
            sync_now,
            configure_sync,
            retry_failed_items,
            dismiss_notes_sync_intro,
            begin_two_way_notes_activation,
            activate_two_way_notes,
            disable_two_way_notes,
            create_latest_notes_backup,
            preview_notes_restore,
            restore_notes_backup,
            preview_complete_resync,
            apply_complete_resync,
            disconnect_account,
            open_mybrewfolio_page,
            updates::get_update_status,
            updates::check_update,
            updates::dismiss_update,
            updates::install_update,
            updates::restart_after_update,
        ])
        .build(tauri::generate_context!())
        .expect("error while running MyBrewFolio Sync")
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = build_application(launched_from_autostart());
    app.run(|app, event| {
        let _ = app;
        match event {
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => show_main_window(app),
            _ => {}
        }
    });
}
