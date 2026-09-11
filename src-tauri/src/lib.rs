mod audio;
mod clipboard;
mod controller;
mod hotkey;
mod macos;
mod model;
mod settings;
mod state;

use controller::Controller;
use serde::Serialize;
use settings::Settings;
use state::AppSnapshot;
use std::fmt::Display;
use std::sync::Arc;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, WindowEvent};
use tauri_plugin_autostart::ManagerExt;

type CommandResult<T> = Result<T, String>;

fn command_result<T, E: Display>(result: Result<T, E>) -> CommandResult<T> {
    result.map_err(|error| format!("{error:#}"))
}

#[tauri::command]
fn get_snapshot(controller: tauri::State<'_, Arc<Controller>>) -> AppSnapshot {
    controller.snapshot()
}

#[tauri::command]
fn get_settings(controller: tauri::State<'_, Arc<Controller>>) -> Settings {
    controller.settings()
}

#[tauri::command]
fn save_settings(
    app: tauri::AppHandle,
    controller: tauri::State<'_, Arc<Controller>>,
    settings: Settings,
) -> CommandResult<()> {
    command_result(controller.save_settings(settings.clone()))?;
    let autostart = app.autolaunch();
    if settings.launch_at_login {
        command_result(autostart.enable())?;
    } else {
        command_result(autostart.disable())?;
    }
    Ok(())
}

#[tauri::command]
fn complete_onboarding(
    app: tauri::AppHandle,
    controller: tauri::State<'_, Arc<Controller>>,
) -> CommandResult<()> {
    command_result(controller.complete_onboarding())?;
    if controller.settings().launch_at_login {
        command_result(app.autolaunch().enable())?;
    }
    Ok(())
}

#[tauri::command]
fn open_accessibility_settings() -> CommandResult<()> {
    let _ = macos::request_accessibility();
    command_result(
        std::process::Command::new("/usr/bin/open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .spawn(),
    )?;
    Ok(())
}

#[tauri::command]
fn open_input_monitoring_settings() -> CommandResult<()> {
    let _ = macos::request_input_monitoring();
    command_result(
        std::process::Command::new("/usr/bin/open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent")
            .spawn(),
    )?;
    Ok(())
}

#[tauri::command]
fn refresh_permissions(controller: tauri::State<'_, Arc<Controller>>) -> AppSnapshot {
    controller.refresh_permissions();
    controller.snapshot()
}

#[tauri::command]
fn list_microphones() -> CommandResult<Vec<String>> {
    command_result(audio::input_devices())
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MicrophoneTestEvent {
    level: f32,
}

#[tauri::command]
async fn test_microphone(app: tauri::AppHandle, device: Option<String>) -> CommandResult<f32> {
    tauri::async_runtime::spawn_blocking(move || {
        let event_app = app.clone();
        let on_level = Arc::new(move |level| {
            let _ = event_app.emit("voxtype://microphone-test", MicrophoneTestEvent { level });
        });
        command_result(audio::test_microphone(device.as_deref(), on_level))
    })
    .await
    .map_err(|error| format!("microphone test task failed: {error}"))?
}

#[tauri::command]
fn start_model_download(controller: tauri::State<'_, Arc<Controller>>) -> CommandResult<()> {
    command_result(controller.inner().clone().start_download())
}

#[tauri::command]
fn cancel_model_download(controller: tauri::State<'_, Arc<Controller>>) {
    controller.cancel_download();
}

#[tauri::command]
fn load_model(controller: tauri::State<'_, Arc<Controller>>) -> CommandResult<()> {
    command_result(controller.load_model())
}

#[tauri::command]
fn unload_model(controller: tauri::State<'_, Arc<Controller>>) -> CommandResult<()> {
    command_result(controller.unload_model())
}

#[tauri::command]
fn remove_model(controller: tauri::State<'_, Arc<Controller>>) -> CommandResult<()> {
    command_result(controller.remove_model())
}

#[tauri::command]
fn retry_paste(controller: tauri::State<'_, Arc<Controller>>) -> CommandResult<()> {
    command_result(controller.inner().clone().retry_paste())
}

#[tauri::command]
fn discard_recovery(controller: tauri::State<'_, Arc<Controller>>) {
    controller.discard_recovery();
}

#[tauri::command]
fn begin_setup_test(controller: tauri::State<'_, Arc<Controller>>) -> CommandResult<()> {
    command_result(controller.inner().clone().begin_setup_test())
}

#[tauri::command]
fn finish_setup_test(controller: tauri::State<'_, Arc<Controller>>) {
    controller.inner().clone().finish_recording();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            #[cfg(target_os = "macos")]
            {
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);
                app.set_dock_visibility(false);
            }

            let controller = Controller::new(app.handle().clone())?;
            hotkey::start(&controller);
            controller.start_privacy_watchdog();
            let snapshot = controller.snapshot();
            let hide_settings = controller.settings().onboarding_complete
                && snapshot.accessibility_trusted
                && snapshot.input_monitoring_trusted;
            app.manage(controller);

            let settings_item = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
            let unload_item = MenuItem::with_id(app, "unload", "Unload Model", true, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit VoxType", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[&settings_item, &unload_item, &separator, &quit_item],
            )?;
            let icon = app
                .default_window_icon()
                .cloned()
                .expect("VoxType bundle icon");
            TrayIconBuilder::with_id("voxtype")
                .icon(icon)
                .icon_as_template(true)
                .title("VoxType")
                .tooltip("VoxType")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "settings" => {
                        if let Some(window) = app.get_webview_window("settings") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "unload" => {
                        if let Some(controller) = app.try_state::<Arc<Controller>>() {
                            let _ = controller.unload_model();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            if hide_settings {
                if let Some(window) = app.get_webview_window("settings") {
                    let _ = window.hide();
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == "settings" {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            get_settings,
            save_settings,
            complete_onboarding,
            open_accessibility_settings,
            open_input_monitoring_settings,
            refresh_permissions,
            list_microphones,
            test_microphone,
            start_model_download,
            cancel_model_download,
            load_model,
            unload_model,
            remove_model,
            retry_paste,
            discard_recovery,
            begin_setup_test,
            finish_setup_test
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
