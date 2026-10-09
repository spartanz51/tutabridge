use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    App, AppHandle, Manager, Window, WindowEvent,
};

const TRAY_ID: &str = "main-tray";

pub fn setup(app: &App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "tray-open", "Open TutaBridge", true, None::<&str>)?;
    let hide = MenuItem::with_id(app, "tray-hide", "Hide to tray", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "tray-quit", "Quit TutaBridge", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &hide, &separator, &quit])?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(
            app.default_window_icon()
                .expect("app icon is configured")
                .clone(),
        )
        .tooltip("TutaBridge")
        // Linux does not emit tray click events. A menu works on all three OSes.
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "tray-open" => show_main_window(app),
            "tray-hide" => {
                if let Some(window) = app.get_webview_window("main") {
                    if let Err(error) = window.hide() {
                        log::warn!("Could not hide the window: {error}");
                    }
                }
            }
            // Explicit Quit (and the native macOS Quit command) still exits.
            "tray-quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

pub fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let result = window
            .show()
            .and_then(|()| window.unminimize())
            .and_then(|()| window.set_focus());
        if let Err(error) = result {
            log::warn!("Could not restore the main window: {error}");
        }
    }
}

pub fn on_window_event(window: &Window, event: &WindowEvent) {
    if window.label() != "main" || window.app_handle().tray_by_id(TRAY_ID).is_none() {
        return;
    }
    if let WindowEvent::CloseRequested { api, .. } = event {
        // A created AppIndicator is not proof that the desktop displays it.
        // Without a tray host (e.g. stock GNOME), keep normal close behaviour.
        if !tray_host_available() {
            return;
        }
        match window.hide() {
            Ok(()) => api.prevent_close(),
            Err(error) => log::warn!("Could not hide to tray; closing normally: {error}"),
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn tray_host_available() -> bool {
    true
}

#[cfg(target_os = "linux")]
fn tray_host_available() -> bool {
    use dbus::blocking::{stdintf::org_freedesktop_dbus::Properties, Connection};
    use std::time::Duration;

    let Ok(connection) = Connection::new_session() else {
        return false;
    };
    let watcher = connection.with_proxy(
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        Duration::from_millis(250),
    );
    watcher
        .get::<bool>(
            "org.kde.StatusNotifierWatcher",
            "IsStatusNotifierHostRegistered",
        )
        .unwrap_or(false)
}
