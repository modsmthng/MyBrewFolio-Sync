// SPDX-License-Identifier: GPL-3.0-or-later

use super::{
    autostart_status_from_state, has_autostart_argument, is_store_managed_build, restart_schedule,
    should_show_reconnect_prompt, update_check_required, update_due, RestartSchedule,
    StoreStartupTaskState, UpdateStatus,
};
use chrono::{Duration, TimeZone, Utc};
use std::sync::atomic::AtomicBool;

#[test]
fn reconnect_prompt_opens_once_for_confirmed_auth_loss_only() {
    let shown = AtomicBool::new(false);
    let mut status = crate::model::AppStatus {
        connected: true,
        machine_host: String::new(),
        machine_reachable: false,
        syncing: false,
        last_sync_at: None,
        last_error: None,
        last_error_code: None,
        last_error_at: None,
        sync_progress: None,
        profiles: 0,
        shots: 0,
        notes: 0,
        conflicts: 0,
        suppressed: 0,
        initial_sync_configured: false,
        duplicate_policy: String::new(),
        notes_sync_status: String::new(),
        notes_sync_target_device_id: None,
        notes_sync_writer_device_id: None,
        this_device_id: None,
        notes_sync_intro_seen: false,
        note_backups: Vec::new(),
        issues: Vec::new(),
    };
    status.connected = false;
    status.last_error_code = Some("MYBREWFOLIO_UNREACHABLE".into());
    assert!(!should_show_reconnect_prompt(&status, &shown));
    status.last_error_code = Some("SYNC_REAUTH_REQUIRED".into());
    assert!(should_show_reconnect_prompt(&status, &shown));
    assert!(!should_show_reconnect_prompt(&status, &shown));
    status.connected = true;
    assert!(!should_show_reconnect_prompt(&status, &shown));
    status.connected = false;
    status.last_error_code = Some("SYNC_DEVICE_REVOKED".into());
    assert!(should_show_reconnect_prompt(&status, &shown));
    status.last_error_code = None;
    assert!(!should_show_reconnect_prompt(&status, &shown));
}

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
    let status = autostart_status_from_state(StoreStartupTaskState::DisabledByPolicy, false);

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

#[test]
fn update_check_runs_daily_or_when_the_saved_time_is_invalid() {
    let now = Utc
        .with_ymd_and_hms(2026, 9, 1, 12, 0, 0)
        .single()
        .expect("test time");
    assert!(update_due(None, now));
    assert!(!update_due(
        Some(&(now - Duration::hours(23)).to_rfc3339()),
        now
    ));
    assert!(update_due(
        Some(&(now - Duration::hours(24)).to_rfc3339()),
        now
    ));
    assert!(update_due(Some("invalid"), now));
    assert!(update_check_required(
        Some(&(now - Duration::hours(1)).to_rfc3339()),
        now,
        true
    ));
}

#[test]
fn update_status_uses_the_frontend_field_names() {
    let status = UpdateStatus::Installed {
        version: "0.4.3".into(),
        restart_requested: true,
        restart_waiting_for_sync: true,
    };
    let json = serde_json::to_value(status).expect("status JSON");
    assert_eq!(json["kind"], "installed");
    assert_eq!(json["restartRequested"], true);
    assert_eq!(json["restartWaitingForSync"], true);
}

#[test]
fn restart_waits_only_for_an_active_sync_cycle() {
    assert_eq!(restart_schedule(false), RestartSchedule::Now);
    assert_eq!(restart_schedule(true), RestartSchedule::WaitForSync);
}
