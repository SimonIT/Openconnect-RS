// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod command;
mod oidc;
mod state;

use command::*;
use state::AppState;
use tauri::Manager;

fn main() {
    #[cfg(target_os = "linux")]
    {
        // TODO: add support for GUI escalation
        sudo::escalate_if_needed().unwrap();
    }

    #[cfg(target_os = "macos")]
    {
        #[cfg(debug_assertions)]
        sudo::escalate_if_needed().unwrap();

        unsafe {
            // TODO: replace with security framework sys bindings
            // https://github.com/kornelski/rust-security-framework
            if libc::geteuid() != 0 && openconnect_core::helper_reluanch_as_root() == 1 {
                std::process::exit(0);
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        use openconnect_core::elevator::windows::{elevate, is_elevated};
        // get command of current execution
        let exe_path = std::env::current_exe().expect("failed to get current executable path");
        let exe_path = exe_path
            .to_str()
            .expect("failed to convert exec path to string");
        let args = std::env::args().skip(1);
        let mut command = std::process::Command::new(exe_path);
        let command = command.args(args);

        if !is_elevated() {
            #[cfg(debug_assertions)]
            const IS_DEBUG: bool = true;

            #[cfg(not(debug_assertions))]
            const IS_DEBUG: bool = false;

            elevate(command, IS_DEBUG).unwrap();
            std::process::exit(0);
        }
    }

    tauri::Builder::default()
        .register_uri_scheme_protocol("oidcvpn", |app, _req| {
            let _app_state: tauri::State<'_, AppState> = app.state();

            tauri::http::ResponseBuilder::new()
                .header("Content-Type", "text/html")
                .status(200)
                .body(b"Authenticated, close this window and return to the application.".to_vec())
        })
        .setup(|app| {
            let vpnc_script = {
                #[cfg(target_os = "windows")]
                {
                    let resource_path = app
                        .path_resolver()
                        .resolve_resource("vpnc-script-win.js")
                        .expect("failed to resolve resource");

                    dunce::canonicalize(resource_path)
                        .expect("failed to canonicalize path")
                        .to_string_lossy()
                        .to_string()
                }

                #[cfg(not(target_os = "windows"))]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let resource_path = app
                        .path_resolver()
                        .resolve_resource("vpnc-script")
                        .expect("failed to resolve resource");

                    let file = std::fs::OpenOptions::new()
                        .write(false)
                        .create(false)
                        .append(false)
                        .read(true)
                        .open(resource_path.clone())
                        .expect("failed to open file");

                    let permissions = file.metadata().unwrap().permissions();
                    let is_executable = permissions.mode() & 0o111 != 0;
                    if !is_executable {
                        let mut permissions = permissions;
                        permissions.set_mode(0o755);
                        file.set_permissions(permissions).unwrap();
                    }

                    resource_path.to_string_lossy().to_string()
                }
            };

            let window = app.get_window("main").expect("no main window");

            #[cfg(any(windows, target_os = "macos"))]
            window_shadows::set_shadow(&window, true).unwrap();

            Ok(tauri::async_runtime::block_on(async {
                AppState::handle_with_vpnc_script(app, &vpnc_script).await
            })?)
        })
        .invoke_handler(tauri::generate_handler![
            disconnect,
            trigger_state_retrieve,
            get_stored_configs,
            upsert_stored_server,
            set_default_server,
            remove_server,
            connect_with_password,
            connect_with_oidc,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
