// SPDX-License-Identifier: GPL-3.0-or-later

pub mod binary;
pub mod cloud;
pub mod credentials;
pub mod engine;
pub mod local;
pub mod model;
pub mod store;

#[cfg(feature = "desktop")]
mod desktop {
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

    use crate::{
        credentials::KeyringCredentialStore, engine::SyncEngine, model::AppStatus, store::AppStore,
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
    use tauri_plugin_updater::UpdaterExt;

    pub(crate) struct TrayStatusItem(MenuItem<tauri::Wry>);
    pub(crate) struct TrayMachineItem(MenuItem<tauri::Wry>);
    pub(crate) struct TrayErrorItem(MenuItem<tauri::Wry>);
    pub(crate) struct TrayAutostartItem(MenuItem<tauri::Wry>);

    async fn emit_status(app: &tauri::AppHandle, engine: &SyncEngine) {
        let status = engine.status().await;
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
        let _ = app.emit("sync-status-changed", status);
    }

    #[cfg(target_os = "windows")]
    const STORE_STARTUP_TASK_ID: &str = "MyBrewFolioSyncStartup";

    #[derive(Debug, Clone, Serialize, PartialEq, Eq)]
    #[serde(rename_all = "camelCase")]
    struct AutostartStatus {
        enabled: bool,
        requires_windows_settings: bool,
        blocked_by_policy: bool,
        migration_available: bool,
    }

    #[cfg(any(target_os = "windows", test))]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum StoreStartupTaskState {
        Enabled,
        Disabled,
        DisabledByUser,
        DisabledByPolicy,
    }

    #[cfg(any(target_os = "windows", test))]
    fn autostart_status_from_state(
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

    fn legacy_autostart_status(enabled: bool) -> AutostartStatus {
        AutostartStatus {
            enabled,
            requires_windows_settings: false,
            blocked_by_policy: false,
            migration_available: false,
        }
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

    #[cfg(target_os = "windows")]
    fn store_startup_task_state(
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
    async fn store_startup_task() -> Result<windows::ApplicationModel::StartupTask, String> {
        use windows::{core::HSTRING, ApplicationModel::StartupTask};

        StartupTask::GetAsync(&HSTRING::from(STORE_STARTUP_TASK_ID))
            .map_err(|error| error.to_string())?
            .get()
            .map_err(|error| error.to_string())
    }

    #[cfg(target_os = "windows")]
    async fn store_autostart_status(legacy_enabled: bool) -> Result<AutostartStatus, String> {
        let task = store_startup_task().await?;
        Ok(autostart_status_from_state(
            store_startup_task_state(task.State().map_err(|error| error.to_string())?),
            legacy_enabled,
        ))
    }

    #[cfg(not(target_os = "windows"))]
    async fn store_autostart_status(_legacy_enabled: bool) -> Result<AutostartStatus, String> {
        Err("Microsoft Store autostart is only available on Windows".into())
    }

    #[cfg(target_os = "windows")]
    async fn request_store_startup_task_enable(
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
    async fn set_store_autostart(
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
    async fn set_store_autostart(
        _app: &tauri::AppHandle,
        _enabled: bool,
        _legacy_enabled: bool,
    ) -> Result<AutostartStatus, String> {
        Err("Microsoft Store autostart is only available on Windows".into())
    }

    async fn autostart_status(app: &tauri::AppHandle) -> Result<AutostartStatus, String> {
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

    async fn set_autostart(
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
    async fn get_autostart_status(app: tauri::AppHandle) -> Result<AutostartStatus, String> {
        autostart_status(&app).await
    }

    #[tauri::command]
    async fn set_autostart_enabled(
        app: tauri::AppHandle,
        enabled: bool,
    ) -> Result<AutostartStatus, String> {
        set_autostart(&app, enabled).await
    }

    fn update_autostart_tray_item(app: &tauri::AppHandle, status: &AutostartStatus) {
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
        emit_status(&app, &engine).await;
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
        // Direct installs retain their existing onboarding behavior. Store builds
        // must ask Windows for explicit startup-task consent from the setting.
        if !store_managed_updates() {
            let _ = app.autolaunch().enable();
        }
        emit_status(&app, &engine).await;
        let engine = engine.inner().clone();
        let handle = app.clone();
        tauri::async_runtime::spawn(async move {
            let _ = engine.sync_once().await;
            emit_status(&handle, &engine).await;
        });
        Ok(())
    }

    #[tauri::command]
    async fn sync_now(
        app: tauri::AppHandle,
        engine: State<'_, Arc<SyncEngine>>,
    ) -> Result<(), String> {
        let result = engine.sync_once().await.map_err(|error| error.to_string());
        emit_status(&app, &engine).await;
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
        emit_status(&app, &engine).await;
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
        emit_status(&app, &engine).await;
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
        emit_status(&app, &engine).await;
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
        emit_status(&app, &engine).await;
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
        emit_status(&app, &engine).await;
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
        emit_status(&app, &engine).await;
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
        emit_status(&app, &engine).await;
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
        emit_status(&app, &engine).await;
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
        engine: State<'_, Arc<SyncEngine>>,
    ) -> Result<serde_json::Value, String> {
        let result = engine
            .disconnect()
            .await
            .map_err(|error| error.to_string())?;
        emit_status(&app, &engine).await;
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

    fn is_store_managed_build(target_is_windows: bool, store_build: Option<&str>) -> bool {
        target_is_windows && matches!(store_build, Some("true"))
    }

    fn store_managed_updates() -> bool {
        is_store_managed_build(
            cfg!(target_os = "windows"),
            option_env!("MYBREWFOLIO_SYNC_WINDOWS_STORE_BUILD"),
        )
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

    #[cfg_attr(mobile, tauri::mobile_entry_point)]
    pub fn run() {
        let mut builder = tauri::Builder::default();
        let launch_in_background = launched_from_autostart();

        #[cfg(desktop)]
        {
            builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
                show_main_window(app);
            }));
        }

        let mut updater = tauri_plugin_updater::Builder::new();
        if let Some(public_key) = option_env!("MYBREWFOLIO_SYNC_UPDATER_PUBLIC_KEY")
            .filter(|value| !value.trim().is_empty())
        {
            updater = updater.pubkey(public_key);
        }

        let autostart = {
            let builder = tauri_plugin_autostart::Builder::new().arg("--autostart");
            #[cfg(target_os = "macos")]
            let builder = builder.macos_launcher(MacosLauncher::LaunchAgent);
            builder.build()
        };

        let app = builder
            .plugin(tauri_plugin_deep_link::init())
            .plugin(tauri_plugin_opener::init())
            .plugin(updater.build())
            .plugin(autostart)
            .setup(move |app| {
                let data_dir = app.path().app_data_dir()?;
                let startup_diagnostics = Arc::new(StartupDiagnostics::new(
                    data_dir.join("startup-diagnostics.log"),
                ));
                startup_diagnostics.reset(&app.package_info().version.to_string());
                app.manage(startup_diagnostics.clone());
                let store = Arc::new(
                    AppStore::open(&data_dir.join("sync.sqlite"))
                        .map_err(|error| error.to_string())?,
                );
                let credentials = Arc::new(KeyringCredentialStore);
                let engine = Arc::new(
                    SyncEngine::open(store.clone(), credentials)
                        .map_err(|error| error.to_string())?,
                );
                app.manage(engine.clone());
                apply_app_icon_visibility(app.handle(), engine.hide_app_icon().unwrap_or(false))?;

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
                    tauri::image::Image::from_bytes(include_bytes!("../icons/tray-template.png"))?;
                #[cfg(not(target_os = "macos"))]
                let tray_icon =
                    tauri::image::Image::from_bytes(include_bytes!("../icons/tray-color.png"))?;
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
                                emit_status(&handle, &engine).await;
                            });
                        }
                        "autostart" => {
                            let handle = app.clone();
                            tauri::async_runtime::spawn(async move {
                                let Ok(current) = autostart_status(&handle).await else {
                                    return;
                                };
                                if let Ok(updated) = set_autostart(&handle, !current.enabled).await
                                {
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

                if !launch_in_background {
                    show_main_window(app.handle());
                }

                let autostart_handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    if let Ok(status) = autostart_status(&autostart_handle).await {
                        update_autostart_tray_item(&autostart_handle, &status);
                    }
                });

                let background_engine = engine.clone();
                let background_handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(8)).await;
                    loop {
                        if background_engine.status().await.connected {
                            let _ = background_engine.sync_once().await;
                            emit_status(&background_handle, &background_engine).await;
                        }
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                });

                let bridge_engine = engine.clone();
                let bridge_handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    loop {
                        if bridge_engine.status().await.connected {
                            if bridge_engine
                                .wait_for_profile_store_operations()
                                .await
                                .is_ok()
                            {
                                emit_status(&bridge_handle, &bridge_engine).await;
                            } else {
                                tokio::time::sleep(Duration::from_secs(2)).await;
                            }
                        } else {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
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
                get_hide_app_icon,
                set_hide_app_icon,
                get_autostart_status,
                set_autostart_enabled,
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

    #[cfg(test)]
    mod tests {
        use super::{
            autostart_status_from_state, has_autostart_argument, is_store_managed_build,
            StoreStartupTaskState,
        };

        #[test]
        fn store_build_is_limited_to_windows_store_packages() {
            assert!(is_store_managed_build(true, Some("true")));
            assert!(!is_store_managed_build(false, Some("true")));
            assert!(!is_store_managed_build(true, Some("false")));
            assert!(!is_store_managed_build(true, None));
        }

        #[test]
        fn legacy_registry_opt_in_is_offered_for_migration() {
            let status = autostart_status_from_state(StoreStartupTaskState::Disabled, true);

            assert!(!status.enabled);
            assert!(status.migration_available);
            assert!(!status.requires_windows_settings);
        }

        #[test]
        fn enabled_startup_has_no_recovery_prompt() {
            let status = autostart_status_from_state(StoreStartupTaskState::Enabled, true);

            assert!(status.enabled);
            assert!(!status.migration_available);
            assert!(!status.requires_windows_settings);
        }

        #[test]
        fn user_disabled_startup_requires_windows_settings() {
            let status = autostart_status_from_state(StoreStartupTaskState::DisabledByUser, true);

            assert!(!status.enabled);
            assert!(status.requires_windows_settings);
            assert!(!status.migration_available);
        }

        #[test]
        fn policy_disabled_startup_is_not_presented_as_user_configurable() {
            let status =
                autostart_status_from_state(StoreStartupTaskState::DisabledByPolicy, false);

            assert!(!status.enabled);
            assert!(status.blocked_by_policy);
            assert!(!status.requires_windows_settings);
        }

        #[test]
        fn autostart_argument_is_detected_without_matching_other_arguments() {
            assert!(has_autostart_argument(["--autostart".to_string()]));
            assert!(!has_autostart_argument([
                "--autostarted".to_string(),
                "--other".to_string(),
            ]));
        }
    }
}

#[cfg(feature = "desktop")]
pub use desktop::run;
