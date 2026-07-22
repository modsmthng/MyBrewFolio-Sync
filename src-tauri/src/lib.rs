// SPDX-License-Identifier: GPL-3.0-or-later

mod binary;
mod cloud;
mod engine;
mod local;
mod model;
mod store;

use std::{sync::Arc, time::Duration};

use engine::SyncEngine;
use model::AppStatus;
use store::AppStore;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, State,
};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_autostart::ManagerExt as AutostartManagerExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;

pub(crate) struct TrayStatusItem(MenuItem<tauri::Wry>);
pub(crate) struct TrayMachineItem(MenuItem<tauri::Wry>);
pub(crate) struct TrayErrorItem(MenuItem<tauri::Wry>);
pub(crate) struct TrayAutostartItem(MenuItem<tauri::Wry>);

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
async fn disconnect_account(
    app: tauri::AppHandle,
    engine: State<'_, Arc<SyncEngine>>,
) -> Result<(), String> {
    engine
        .disconnect()
        .await
        .map_err(|error| error.to_string())?;
    engine.emit_status(&app).await;
    Ok(())
}

#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        let _ = app;
        return Ok("store-managed".into());
    }
    #[cfg(not(target_os = "windows"))]
    {
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

    builder
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(updater.build())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
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
            TrayIconBuilder::new()
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
                        let engine = app.state::<Arc<SyncEngine>>().inner().clone();
                        let handle = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = engine.disconnect().await;
                            engine.emit_status(&handle).await;
                        });
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
            get_status,
            set_machine_host,
            begin_oauth,
            complete_oauth,
            sync_now,
            disconnect_account,
            install_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running MyBrewFolio Sync");
}
