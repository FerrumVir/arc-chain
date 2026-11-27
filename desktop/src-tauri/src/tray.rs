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

    let menu = Menu::with_items(app, &[&open_i, &sep1, &status_i, &round_i, &sep2, &quit_i])?;

    let menu_on_left_click = cfg!(target_os = "linux");

    let tray = TrayIconBuilder::with_id("main")
        .tooltip("ARC Node")
        .menu(&menu)
        .show_menu_on_left_click(menu_on_left_click)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => {
                crate::desktop_instance::show_main(app);
            }
            "quit" => {
                crate::desktop_instance::begin_closing(app);
                crate::sharing_awake::set_enabled(false);
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(state) = handle.try_state::<AppState>() {
                        let _ = crate::commands::stop_node_inner(&state).await;
                    }
                    crate::sharing_awake::set_enabled(false);
                    handle.exit(0);
                });
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                crate::desktop_instance::show_main(app);
            }
        })
        .icon(app.default_window_icon().cloned().unwrap_or_else(|| {
            tauri::image::Image::new(&[0; 4], 1, 1)
        }))
        .build(app)?;
    let _ = tray;

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
            if matches!(
                *state.data_migration_error.lock().await,
                Some(crate::start_fence::StartFence::OtherDesktop)
            ) {
                break;
            }
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
                    let v: serde_json::Value = r.json().await.unwrap_or(serde_json::Value::Null);
                    let peers = v.get("peers").and_then(|x| x.as_u64()).unwrap_or(0);
                    let round = v.get("dag_round").and_then(|x| x.as_u64()).unwrap_or(0);
                    let status = if peers >= 1 {
                        format!("Status: running · {} peers", peers)
                    } else {
                        "Status: running".to_string()
                    };
                    (status, format!("Round: {}", round))
                }
                _ => (
                    "Status: node off or starting".to_string(),
                    "Round: -".to_string(),
                ),
            };
            let _ = status_item.set_text(status_text);
            let _ = round_item.set_text(round_text);
        }
    });

    Ok(())
}
