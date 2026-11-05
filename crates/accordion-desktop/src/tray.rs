//! The icon in the system tray, which shows how syncing is going, and its
//! menu.

use tauri::AppHandle;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

use crate::syncing::AccountView;

const ID: &str = "main";

pub fn create(app: &AppHandle, accounts: &[AccountView]) -> tauri::Result<()> {
    let mut tray = TrayIconBuilder::with_id(ID)
        .tooltip("Accordion")
        .menu(&menu(app, accounts)?)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => crate::show_window(app),
            "quit" => app.exit(0),
            id => {
                if let Some(account) = id.strip_prefix("open:") {
                    crate::open_folder_of(app, account);
                }
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                crate::show_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn menu(app: &AppHandle, accounts: &[AccountView]) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;
    menu.append(&MenuItem::with_id(
        app,
        "show",
        "Open Accordion",
        true,
        None::<&str>,
    )?)?;
    for view in accounts {
        menu.append(&MenuItem::with_id(
            app,
            format!("open:{}", view.account.id),
            format!("Open Accordion ({}) in File Explorer", view.account.name),
            true,
            None::<&str>,
        )?)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        "quit",
        "Quit Accordion",
        true,
        None::<&str>,
    )?)?;
    Ok(menu)
}

/// Shows how syncing is going in the icon's tooltip, and keeps the menu's
/// accounts up to date.
pub fn update(app: &AppHandle, accounts: &[AccountView]) {
    let Some(tray) = app.tray_by_id(ID) else {
        return;
    };
    let tooltip = summary(accounts);
    let _ = tray.set_tooltip(Some(&tooltip));
    if let Ok(menu) = menu(app, accounts) {
        let _ = tray.set_menu(Some(menu));
    }
}

fn summary(accounts: &[AccountView]) -> String {
    if accounts.is_empty() {
        return "Accordion: add an account to start syncing".into();
    }
    let trouble: Vec<&str> = accounts
        .iter()
        .filter(|view| matches!(view.status.state, "offline" | "signed_out" | "problem"))
        .map(|view| view.account.name.as_str())
        .collect();
    if !trouble.is_empty() {
        return format!("Accordion: can't sync {}", trouble.join(", "));
    }
    if accounts.iter().any(|view| view.status.paused_deletes > 0) {
        return "Accordion: waiting for you to allow deletes".into();
    }
    if accounts.iter().any(|view| view.status.state == "starting") {
        return "Accordion: starting".into();
    }
    "Accordion: up to date".into()
}
