#![cfg(all(desktop, not(test)))]
use tauri::{
  menu::{Menu, MenuItem},
  tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
  Manager, Runtime,
};

pub fn create_tray<R: Runtime>(app: &tauri::AppHandle<R>) -> tauri::Result<()> {
  let toggle_i = MenuItem::with_id(app, "toggle", "Show/Hide", true, None::<&str>)?;
  let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
  let tray_menu = Menu::with_items(
    app,
    &[
      &toggle_i,
      &quit_i,
    ],
  )?;

  let _ = TrayIconBuilder::with_id("tray-1")
    .tooltip("Tacho Bridge Application")
    .icon(app.default_window_icon().unwrap().clone())
    .menu(&tray_menu)
    .show_menu_on_left_click(false)
    .on_menu_event(move |app, event| match event.id.as_ref() {
      "quit" => {
        app.exit(0);
      }
      "toggle" => {
        if let Some(window) = app.get_webview_window("main") {
          let new_title = if window.is_visible().unwrap_or_default() {
            let _ = window.hide();
            "Show/Hide"
          } else {
            let _ = window.show();
            let _ = window.set_focus();
            "Show/Hide"
          };
          toggle_i.set_text(new_title).unwrap();
        }
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
        if let Some(window) = app.get_webview_window("main") {
          let _ = window.show();
          let _ = window.set_focus();
        }
      }
    })
    .build(app);

  Ok(())
}