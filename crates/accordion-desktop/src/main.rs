//! The Accordion desktop app (decision 23 in the `accordion` repo's
//! `docs/decisions`). It lives in the system tray, and keeps each
//! account's Drive in sync. On Windows, each account is in File Explorer's
//! navigation pane, and files download when you open them.

// No console window on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod accounts;
mod autostart;
#[cfg(windows)]
mod files;
mod syncing;
mod tray;

use std::sync::Arc;

use accordion_sync::Reach;
use accordion_sync::add::add as add_with_code;
use accordion_sync::oauth::{DESKTOP_CLIENT_ID, Pending};
use tauri::{AppHandle, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder};

use crate::syncing::{AccountView, App};

fn main() {
    let background = std::env::args().any(|arg| arg == autostart::BACKGROUND);
    tauri::Builder::default()
        // Starting the app again, such as from the Start menu, opens the
        // window of the one that's running.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_window(app);
        }))
        .setup(move |app| {
            let state = App::new(app.handle().clone()).map_err(std::io::Error::other)?;
            app.manage(Arc::clone(&state));
            tray::create(app.handle(), &state.views())?;
            state.start_all();
            if !background || state.accounts().is_empty() {
                show_window(app.handle());
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list,
            sign_in,
            cancel_sign_in,
            use_code,
            remove,
            open_folder,
            sync_now,
            dismiss_notices,
            allow_deletes,
            bring_back,
            suggested_name,
        ])
        .build(tauri::generate_context!())
        .expect("the app starts")
        .run(|_app, event| {
            // Closing the window leaves the app syncing in the tray. Only
            // **Quit Accordion** ends it.
            if let RunEvent::ExitRequested {
                api, code: None, ..
            } = event
            {
                api.prevent_exit();
            }
        });
}

/// Opens the app's window, or brings it to the front.
pub fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("Accordion")
        .inner_size(520.0, 660.0)
        .min_inner_size(400.0, 480.0)
        .center()
        .build();
}

/// Opens an account's folder in File Explorer.
pub fn open_folder_of(app: &AppHandle, id: &str) {
    let state = app.state::<Arc<App>>();
    if let Some(account) = state.account(id) {
        open(account.folder.as_os_str());
    }
}

/// Opens a folder in File Explorer, or an address in the default browser.
/// `explorer.exe` mistakes a long address with `&` in it for a folder and
/// opens Documents, so Windows' shell opens both instead.
#[cfg(windows)]
fn open(target: &std::ffi::OsStr) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};

    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &HSTRING::from(target),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

#[cfg(not(windows))]
fn open(target: &std::ffi::OsStr) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    let _ = std::process::Command::new(program).arg(target).spawn();
}

#[tauri::command]
fn list(state: State<'_, Arc<App>>) -> Vec<AccountView> {
    state.views()
}

/// The name the add form suggests: "Home" for the first account.
#[tauri::command]
fn suggested_name(state: State<'_, Arc<App>>) -> String {
    if state.accounts().is_empty() {
        "Home".into()
    } else {
        String::new()
    }
}

fn check_name(state: &App, name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Enter a name for this account, such as Home or Work.".into());
    }
    if name.contains(['<', '>', ':', '"', '/', '\\', '|', '?', '*']) {
        return Err("A name can't have any of these: < > : \" / \\ | ? *".into());
    }
    if state
        .accounts()
        .iter()
        .any(|account| account.name.eq_ignore_ascii_case(name))
    {
        return Err(format!(
            "There's already an account named {name}. Choose another name."
        ));
    }
    Ok(())
}

/// Adds an account by signing in through the browser: Accordion's sign-in
/// page opens, and the person allows this computer there.
#[tauri::command]
async fn sign_in(
    state: State<'_, Arc<App>>,
    server: String,
    name: String,
) -> Result<AccountView, String> {
    check_name(&state, &name)?;
    let device = accounts::device_name();
    let server = with_scheme(&server);
    let pending = Pending::start_as(DESKTOP_CLIENT_ID, &server, &device, Reach::default())
        .await
        .map_err(|err| err.to_string())?;
    open(std::ffi::OsStr::new(&pending.url));
    let added = tokio::select! {
        added = pending.finish() => added.map_err(|err| err.to_string())?,
        () = state.cancel_sign_in.notified() => {
            return Err("Signing in was canceled.".into());
        }
    };
    state.add(&name, device, added)
}

#[tauri::command]
fn cancel_sign_in(state: State<'_, Arc<App>>) {
    state.cancel_sign_in.notify_waiters();
}

/// Adds an account with a code from Accordion's Settings, or its whole
/// setup, such as away from home.
#[tauri::command]
async fn use_code(
    state: State<'_, Arc<App>>,
    server: String,
    code: String,
    name: String,
) -> Result<AccountView, String> {
    check_name(&state, &name)?;
    let device = accounts::device_name();
    let server = server.trim();
    let server = (!server.is_empty()).then(|| with_scheme(server));
    let added = add_with_code(code.trim(), server.as_deref(), &device, Reach::default())
        .await
        .map_err(|err| err.to_string())?;
    state.add(&name, device, added)
}

/// An address as typed, such as `ourhome.dedyn.io`, with `https://`.
fn with_scheme(server: &str) -> String {
    let server = server.trim().trim_end_matches('/');
    if server.contains("://") {
        server.to_owned()
    } else {
        format!("https://{server}")
    }
}

#[tauri::command]
async fn remove(state: State<'_, Arc<App>>, id: String) -> Result<(), String> {
    state.remove(&id).await
}

#[tauri::command]
fn open_folder(app: AppHandle, id: String) {
    open_folder_of(&app, &id);
}

#[tauri::command]
fn sync_now(state: State<'_, Arc<App>>, id: String) {
    state.sync_now(&id);
}

#[tauri::command]
fn dismiss_notices(state: State<'_, Arc<App>>, id: String) {
    state.dismiss_notices(&id);
}

#[tauri::command]
fn allow_deletes(state: State<'_, Arc<App>>, id: String) {
    state.allow_deletes(&id);
}

#[tauri::command]
fn bring_back(state: State<'_, Arc<App>>, id: String) {
    state.bring_back(&id);
}
