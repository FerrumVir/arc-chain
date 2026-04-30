// System tray for ARC Node.
//
// The desktop app is designed to run continuously on a user's machine -
// closing the window hides to the tray; the only way to actually stop
// arc-node is the "Quit" menu item here. Pattern matches Tailscale,
// Docker Desktop, Ollama.
//
// Menu layout:
//   ARC Node                         (disabled header)
//   -----
//   Open                             show the window
//   -----
//   Status: <health>                 disabled, updated via ticker
//   Round: <dag_round>               disabled, updated via ticker
//   -----
//   Quit ARC Node                    stop arc-node + exit app
//
// The status/round labels refresh every 5 s via a spawned tokio task
// that pokes /health on the local node.

use crate::AppState;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager,
};

pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let open_i = MenuItem::with_id(app, "open", "Open ARC Node", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let status_i = MenuItem::with_id(app, "status", "Status: starting…", false, None::<&str>)?;
    let round_i = MenuItem::with_id(app, "round", "Round: -", false, None::<&str>)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let quit_i = MenuItem::with_id(app, "quit", "Quit ARC Node", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[&open_i, &sep1, &status_i, &round_i, &sep2, &quit_i],
    )?;

    let tray = TrayIconBuilder::with_id("main")
        .tooltip("ARC Node")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => {
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.set_focus();
                    let _ = win.unminimize();
                }
            }
            "quit" => {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    // Stop arc-node cleanly before exiting; the window-close
                    // hide-to-tray pattern means the node is otherwise left
                    // running across window close.
                    if let Some(state) = handle.try_state::<AppState>() {
                        let mut node = state.node.lock().await;
                        let _ = node.stop().await;
                    }
                    handle.exit(0);
                });
            }
            _ => {}
        })
        // Left-clicking the tray icon pops open the window (macOS menu
        // bar convention) rather than showing the menu (which right-click
        // still does).
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.set_focus();
                    let _ = win.unminimize();
                }
            }
        })
        .icon(app.default_window_icon().cloned().unwrap_or_else(|| {
            // Fallback: a 1px transparent PNG. Build is guaranteed to have
            // the window icon, so this branch should never fire.
            tauri::image::Image::new(&[0; 4], 1, 1)
        }))
        .build(app)?;
    let _ = tray;

    // Spawn a background refresh loop that updates the Status / Round
    // items every 5 s from /health on the local node. Cheap: one HTTP
    // round-trip to 127.0.0.1.
    let handle = app.clone();
    let status_item = status_i.clone();
    let round_item = round_i.clone();
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let state = match handle.try_state::<AppState>() {
                Some(s) => s,
                None => continue,
            };
            let port = state.node.lock().await.rpc_port;
            let url = format!("http://127.0.0.1:{}/health", port);
            let resp = state
                .http
                .get(&url)
                .timeout(std::time::Duration::from_secs(3))
                .send()
                .await;
            let (status_text, round_text) = match resp {
                Ok(r) if r.status().is_success() => {
                    let v: serde_json::Value =
                        r.json().await.unwrap_or(serde_json::Value::Null);
                    let peers =
                        v.get("peers").and_then(|x| x.as_u64()).unwrap_or(0);
                    let uptime = v
                        .get("uptime_secs")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    let round = v
                        .get("dag_round")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    let health = if peers == 0 || uptime < 8 {
                        "syncing"
                    } else {
                        "live"
                    };
                    (
                        format!("Status: {} · {} peers", health, peers),
                        format!("Round: {}", round),
                    )
                }
                _ => (
                    "Status: offline".to_string(),
                    "Round: -".to_string(),
                ),
            };
            let _ = status_item.set_text(status_text);
            let _ = round_item.set_text(round_text);
        }
    });

    Ok(())
}
