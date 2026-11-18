#[cfg(desktop)]
use tauri::{
    Emitter,
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();

    #[cfg(desktop)]
    let builder = builder
        .menu(|app| {
            let open_rom = MenuItem::with_id(
                app,
                "open-rom",
                "Abrir arquivos…",
                true,
                Some("CmdOrCtrl+O"),
            )?;
            let back_to_library = MenuItem::with_id(
                app,
                "library",
                "Voltar à Biblioteca",
                true,
                Some("CmdOrCtrl+1"),
            )?;
            let quit = PredefinedMenuItem::quit(app, Some("Sair"))?;
            let separator = PredefinedMenuItem::separator(app)?;
            let show_help = MenuItem::with_id(app, "help", "Ajuda do Emmu", true, None::<&str>)?;
            let configure_controls = MenuItem::with_id(
                app,
                "configure-controls",
                "Configurar controles…",
                true,
                None::<&str>,
            )?;

            let file_menu = Submenu::with_items(
                app,
                "Arquivo",
                true,
                &[&open_rom, &back_to_library, &separator, &quit],
            )?;
            let emulation_menu =
                Submenu::with_items(app, "Emulação", true, &[&configure_controls])?;
            let help_menu = Submenu::with_items(app, "Ajuda", true, &[&show_help])?;
            Menu::with_items(app, &[&file_menu, &emulation_menu, &help_menu])
        })
        .on_menu_event(|app, event| {
            let event_name = match event.id().as_ref() {
                "open-rom" => Some("open-rom-request"),
                "library" => Some("return-to-library"),
                "configure-controls" => Some("configure-controls"),
                "help" => Some("show-help"),
                _ => None,
            };
            if let Some(event_name) = event_name {
                if let Err(error) = app.emit(event_name, ()) {
                    eprintln!("Failed to dispatch menu event {event_name}: {error}");
                }
            }
        });

    builder
        .run(tauri::generate_context!())
        .expect("failed to run Emmu application");
}
