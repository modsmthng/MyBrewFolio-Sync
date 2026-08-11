// SPDX-License-Identifier: GPL-3.0-or-later

mod binary;
mod cloud;
mod engine;
mod local;
mod model;
mod store;

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use engine::SyncEngine;
use model::AppStatus;
use store::AppStore;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager, State,
};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_autostart::ManagerExt as AutostartManagerExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;

pub(crate) struct TrayStatusItem(MenuItem<tauri::Wry>);
pub(crate) struct TrayMachineItem(MenuItem<tauri::Wry>);
pub(crate) struct TrayErrorItem(MenuItem<tauri::Wry>);
pub(crate) struct TrayAutostartItem(MenuItem<tauri::Wry>);

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

#[cfg(target_os = "windows")]
fn windows_version() -> String {
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
fn windows_version() -> String {
    std::env::consts::OS.into()
}

#[tauri::command]
fn frontend_ready(diagnostics: State<'_, Arc<StartupDiagnostics>>) {
    diagnostics.mark_frontend_ready();
}

#[tauri::command]
async fn get_status(engine: State<'_, Arc<SyncEngine>>) -> Result<AppStatus, String> {
    Ok(engine.status().await)
}

#[tauri::command]
async fn set_machine_host(
    app: tauri::AppHandle,
    engine: State<'_, Arc<SyncEngine>>,
    host: String,
) -> Result<(), String> {
    engine
        .set_host(&host)
        .await
        .map_err(|error| error.to_string())?;
    engine.emit_status(&app).await;
    Ok(())
}

#[tauri::command]
async fn begin_oauth(
    app: tauri::AppHandle,
    engine: State<'_, Arc<SyncEngine>>,
) -> Result<(), String> {
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
    callback_url: String,
) -> Result<(), String> {
    engine
        .complete_oauth(&callback_url)
        .await
        .map_err(|error| error.to_string())?;
    // The onboarding promises background synchronization after setup. A user
    // can turn this off at any time from the dashboard or tray application.
    let _ = app.autolaunch().enable();
    engine.emit_status(&app).await;
    let engine = engine.inner().clone();
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = engine.sync_once().await;
        engine.emit_status(&handle).await;
    });
    Ok(())
}

#[tauri::command]
async fn sync_now(app: tauri::AppHandle, engine: State<'_, Arc<SyncEngine>>) -> Result<(), String> {
    let result = engine.sync_once().await.map_err(|error| error.to_string());
    engine.emit_status(&app).await;
    result
}

#[tauri::command]
async fn configure_sync(
    app: tauri::AppHandle,
    engine: State<'_, Arc<SyncEngine>>,
    reuse_matching: bool,
) -> Result<(), String> {
    engine
        .configure_sync(reuse_matching)
        .await
        .map_err(|error| error.to_string())?;
    let result = engine.sync_once().await.map_err(|error| error.to_string());
    engine.emit_status(&app).await;
    result
}

#[tauri::command]
async fn retry_failed_items(
    app: tauri::AppHandle,
    engine: State<'_, Arc<SyncEngine>>,
) -> Result<(), String> {
    engine
        .retry_failures()
        .await
        .map_err(|error| error.to_string())?;
    let result = engine.sync_once().await.map_err(|error| error.to_string());
    engine.emit_status(&app).await;
    result
}

#[tauri::command]
async fn dismiss_notes_sync_intro(
    app: tauri::AppHandle,
    engine: State<'_, Arc<SyncEngine>>,
) -> Result<(), String> {
    engine
        .dismiss_notes_sync_intro()
        .await
        .map_err(|error| error.to_string())?;
    engine.emit_status(&app).await;
    Ok(())
}

#[tauri::command]
async fn begin_two_way_notes_activation(
    app: tauri::AppHandle,
    engine: State<'_, Arc<SyncEngine>>,
) -> Result<serde_json::Value, String> {
    let result = engine
        .begin_two_way_notes_activation()
        .await
        .map_err(|error| error.to_string())?;
    engine.emit_status(&app).await;
    Ok(result)
}

#[tauri::command]
async fn activate_two_way_notes(
    app: tauri::AppHandle,
    engine: State<'_, Arc<SyncEngine>>,
    backup_id: String,
    decisions: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let result = engine
        .activate_two_way_notes(&backup_id, decisions)
        .await
        .map_err(|error| error.to_string())?;
    engine.emit_status(&app).await;
    Ok(result)
}

#[tauri::command]
async fn disable_two_way_notes(
    app: tauri::AppHandle,
    engine: State<'_, Arc<SyncEngine>>,
) -> Result<(), String> {
    engine
        .disable_two_way_notes()
        .await
        .map_err(|error| error.to_string())?;
    engine.emit_status(&app).await;
    Ok(())
}

#[tauri::command]
async fn create_latest_notes_backup(
    app: tauri::AppHandle,
    engine: State<'_, Arc<SyncEngine>>,
) -> Result<String, String> {
    let result = engine
        .create_latest_notes_backup()
        .await
        .map_err(|error| error.to_string())?;
    engine.emit_status(&app).await;
    Ok(result)
}

#[tauri::command]
async fn preview_notes_restore(
    engine: State<'_, Arc<SyncEngine>>,
    backup_id: String,
) -> Result<serde_json::Value, String> {
    engine
        .preview_notes_restore(&backup_id)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn restore_notes_backup(
    app: tauri::AppHandle,
    engine: State<'_, Arc<SyncEngine>>,
    backup_id: String,
    source_keys: Vec<String>,
) -> Result<serde_json::Value, String> {
    let result = engine
        .restore_notes_backup(&backup_id, &source_keys)
        .await
        .map_err(|error| error.to_string())?;
    engine.emit_status(&app).await;
    Ok(result)
}

#[tauri::command]
async fn preview_complete_resync(
    engine: State<'_, Arc<SyncEngine>>,
) -> Result<serde_json::Value, String> {
    engine
        .resync_preview()
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn apply_complete_resync(
    app: tauri::AppHandle,
    engine: State<'_, Arc<SyncEngine>>,
    decisions: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let applied = engine
        .apply_resync(decisions)
        .await
        .map_err(|error| error.to_string())?;
    let follow_up_error = engine
        .sync_once()
        .await
        .err()
        .map(|error| error.to_string());
    engine.emit_status(&app).await;
    let mut result = applied;
    if let (Some(object), Some(error)) = (result.as_object_mut(), follow_up_error) {
        object.insert("followUpError".into(), serde_json::Value::String(error));
    }
    Ok(result)
}

#[tauri::command]
async fn disconnect_account(
    app: tauri::AppHandle,
    engine: State<'_, Arc<SyncEngine>>,
) -> Result<serde_json::Value, String> {
    let result = engine
        .disconnect()
        .await
        .map_err(|error| error.to_string())?;
    engine.emit_status(&app).await;
    Ok(result)
}

#[tauri::command]
fn open_mybrewfolio_page(app: tauri::AppHandle, page: String) -> Result<(), String> {
    let url = match page.as_str() {
        "syncHelp" => "https://mybrewfolio.com/support/sync",
        "privacy" => "https://mybrewfolio.com/legal/privacy",
        "accountSync" => "https://mybrewfolio.com/account/sync",
        _ => return Err("Unknown MyBrewFolio page".into()),
    };
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn check_update(app: tauri::AppHandle) -> Result<String, String> {
    if store_managed_updates() {
        return Ok("store-managed".into());
    }
    let public_key = option_env!("MYBREWFOLIO_SYNC_UPDATER_PUBLIC_KEY")
        .unwrap_or("")
        .trim();
    if public_key.is_empty() {
        return Ok("not-configured".into());
    }
    let updater = app.updater().map_err(|error| error.to_string())?;
    let Some(update) = updater.check().await.map_err(|error| error.to_string())? else {
        return Ok("up-to-date".into());
    };
    Ok(format!("available:{}", update.version))
}

#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<String, String> {
    if store_managed_updates() {
        return Ok("store-managed".into());
    }
    let public_key = option_env!("MYBREWFOLIO_SYNC_UPDATER_PUBLIC_KEY")
        .unwrap_or("")
        .trim();
    if public_key.is_empty() {
        return Ok("not-configured".into());
    }
    let updater = app.updater().map_err(|error| error.to_string())?;
    let Some(update) = updater.check().await.map_err(|error| error.to_string())? else {
        return Ok("up-to-date".into());
    };
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|error| error.to_string())?;
    Ok("installed".into())
}

fn store_managed_updates() -> bool {
    cfg!(target_os = "windows")
        && matches!(
            option_env!("MYBREWFOLIO_SYNC_WINDOWS_STORE_BUILD"),
            Some("true")
        )
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
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

    let app = builder
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(updater.build())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let startup_diagnostics = Arc::new(StartupDiagnostics::new(
                data_dir.join("startup-diagnostics.log"),
            ));
            startup_diagnostics.reset(&app.package_info().version.to_string());
            app.manage(startup_diagnostics.clone());
            let store = Arc::new(
                AppStore::open(&data_dir.join("sync.sqlite")).map_err(|error| error.to_string())?,
            );
            let engine =
                Arc::new(SyncEngine::open(store.clone()).map_err(|error| error.to_string())?);
            app.manage(engine.clone());

            #[cfg(any(target_os = "linux", all(debug_assertions, windows)))]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                app.deep_link().register_all()?;
            }

            let status_item =
                MenuItem::with_id(app, "status", "MyBrewFolio Sync", false, None::<&str>)?;
            app.manage(TrayStatusItem(status_item.clone()));
            let machine_host = store
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
            let error_item =
                MenuItem::with_id(app, "error", "No Sync errors", false, None::<&str>)?;
            app.manage(TrayErrorItem(error_item.clone()));
            let show_item = MenuItem::with_id(app, "show", "Open Sync", true, None::<&str>)?;
            let sync_item = MenuItem::with_id(app, "sync", "Sync now", true, None::<&str>)?;
            let autostart_enabled = app.autolaunch().is_enabled().unwrap_or(false);
            let autostart_item = MenuItem::with_id(
                app,
                "autostart",
                if autostart_enabled {
                    "Disable start with computer"
                } else {
                    "Start with computer"
                },
                true,
                None::<&str>,
            )?;
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
            let tray_icon =
                tauri::image::Image::from_bytes(include_bytes!("../icons/tray-template.png"))?;
            TrayIconBuilder::new()
                .icon(tray_icon)
                .icon_as_template(cfg!(target_os = "macos"))
                .tooltip("MyBrewFolio Sync")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main_window(app),
                    "sync" => {
                        let engine = app.state::<Arc<SyncEngine>>().inner().clone();
                        let handle = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = engine.sync_once().await;
                            engine.emit_status(&handle).await;
                        });
                    }
                    "autostart" => {
                        let autostart = app.autolaunch();
                        let enabled = autostart.is_enabled().unwrap_or(false);
                        let result = if enabled {
                            autostart.disable()
                        } else {
                            autostart.enable()
                        };
                        if result.is_ok() {
                            if let Some(item) = app.try_state::<TrayAutostartItem>() {
                                let _ = item.0.set_text(if enabled {
                                    "Start with computer"
                                } else {
                                    "Disable start with computer"
                                });
                            }
                        }
                    }
                    "disconnect" => {
                        show_main_window(app);
                        let _ = app.emit("disconnect-confirmation-requested", ());
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            let background_engine = engine.clone();
            let background_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_secs(8)).await;
                loop {
                    if background_engine.status().await.connected {
                        let _ = background_engine.sync_once().await;
                        background_engine.emit_status(&background_handle).await;
                    }
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            });

            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_secs(15)).await;
                if !startup_diagnostics.frontend_ready.load(Ordering::SeqCst) {
                    startup_diagnostics.append(&format!(
                        "frontend_timeout=true\nfrontend_timeout_utc={}",
                        chrono::Utc::now().to_rfc3339()
                    ));
                }
            });

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
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            frontend_ready,
            get_status,
            set_machine_host,
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
            check_update,
            install_update,
        ])
        .build(tauri::generate_context!())
        .expect("error while running MyBrewFolio Sync");

    app.run(|app, event| {
        let _ = app;
        match event {
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => show_main_window(app),
            _ => {}
        }
    });
}
