/** emmu/platforms/desktop/src/lib.rs */
use emmu_mesen_sys::{
    DebugControllerState, InitDll, InitializeEmu, IsRunning, LoadRom, NOTIFICATION_PPU_FRAME_DONE,
    NOTIFICATION_REFRESH_SOFTWARE_RENDERER, RegisterNotificationCallback, Release,
    SoftwareRendererFrameAbi, Stop, UnregisterNotificationCallback, configure_audio,
    connect_gameboy_controllers, connect_gameboy_dmg_controllers, connect_gba_controllers,
    connect_nes_controllers, connect_pce_controllers, connect_snes_controllers,
    initialize_debugger, install_gba_bios, is_shutting_down, load_state_from_file,
    mark_shutting_down, reset, save_state_to_file, set_audio_volume, set_input_overrides,
};
use serde::Serialize;
use std::ffi::CString;
use std::fs;
use std::os::raw::{c_int, c_void};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, Once};
use tauri::State;
#[cfg(desktop)]
use tauri::{
    Emitter,
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
};

const NES_WIDTH: usize = 256;
/// Tamanho real do buffer do PPU NES (`NesConstants::ScreenPixelCount`).
const NES_FRAME_HEIGHT: usize = 240;
const SNES_FRAME_HEIGHT: usize = 224;
const GBA_WIDTH: usize = 240;
const GBA_FRAME_HEIGHT: usize = 160;
const GAMEBOY_WIDTH: usize = 160;
const GAMEBOY_FRAME_HEIGHT: usize = 144;
/// Mesen publica o frame PCE em 256x242 (o mesmo conteúdo do core WASM, sem o
/// overscan horizontal esquerdo/direito). O callback de notificação informa
/// as dimensões reais, então estes valores são apenas o ponto de partida do
/// canvas antes do primeiro frame.
const PCE_WIDTH: usize = 256;
const PCE_FRAME_HEIGHT: usize = 242;

const NES_PALETTE_64: [u32; 64] = [
    0x00545454, 0x00001E74, 0x00081090, 0x00300088, 0x00440064, 0x005C0030, 0x00540400, 0x003C1800,
    0x00202A00, 0x00083A00, 0x00004000, 0x00003C00, 0x0000323C, 0x00000000, 0x00000000, 0x00000000,
    0x00989698, 0x00084CC4, 0x003032EC, 0x005C1EE4, 0x008814B0, 0x00A01464, 0x00982220, 0x00783C00,
    0x00545A00, 0x00287200, 0x00087C00, 0x00007628, 0x00006678, 0x00000000, 0x00000000, 0x00000000,
    0x00ECEEEC, 0x004C9AEC, 0x00787CEC, 0x00B062EC, 0x00E454EC, 0x00EC58B4, 0x00EC6A64, 0x00D48820,
    0x00A6AA00, 0x0066C400, 0x0032D014, 0x0014C868, 0x0024B4C4, 0x003C3C3C, 0x00000000, 0x00000000,
    0x00FFFFFF, 0x00A8DCEC, 0x00BCCCFC, 0x00D4B2FC, 0x00ECAEFC, 0x00F4AEB4, 0x00FCB8AE, 0x00F4C87C,
    0x00DCE05C, 0x00B4EC5C, 0x0094F47C, 0x0078ECAC, 0x0080E4E8, 0x00A0A2A0, 0x00000000, 0x00000000,
];

struct MesenFrameBuffer {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    has_frame: bool,
}

static LATEST_MESEN_FRAME: Mutex<MesenFrameBuffer> = Mutex::new(MesenFrameBuffer {
    rgba: Vec::new(),
    width: NES_WIDTH as u32,
    height: NES_FRAME_HEIGHT as u32,
    has_frame: false,
});

const fn empty_pad_state() -> DebugControllerState {
    DebugControllerState {
        a: false,
        b: false,
        x: false,
        y: false,
        l: false,
        r: false,
        u: false,
        d: false,
        up: false,
        down: false,
        left: false,
        right: false,
        select: false,
        start: false,
    }
}

static ACTIVE_CONTROLLERS: Mutex<[DebugControllerState; 2]> =
    Mutex::new([empty_pad_state(), empty_pad_state()]);

static MESEN_INIT_ONCE: Once = Once::new();
static MESEN_LISTENER: Mutex<usize> = Mutex::new(0);
/// Garante que `Release()` rode uma única vez, mesmo se o evento de saída do
/// Tauri e o comando `mesen_release` do JS chegarem no mesmo instante.
static MESEN_RELEASED: AtomicBool = AtomicBool::new(false);

/// Dimensões do frame capturadas no carregamento da ROM. O callback de
/// notificação não pode chamar `GetBaseScreenSize()` (FFI): durante o
/// encerramento o `VideoDecoder` já pode ter sido destruído e a chamada
/// causa EXC_BAD_ACCESS em `VideoDecoder::GetBaseFrameInfo`.
static MESEN_FRAME_WIDTH: AtomicU32 = AtomicU32::new(NES_WIDTH as u32);
static MESEN_FRAME_HEIGHT: AtomicU32 = AtomicU32::new(240);
/// NES publica o frame no `PpuFrameDone`; SNES, GBA e GBC não passam buffer
/// nessa notificação e usam o `RefreshSoftwareRenderer`. O flag evita ler o
/// parâmetro da notificação 7 como se fosse um buffer de paleta nesses consoles.
static ACTIVE_CONSOLE_IS_NES: AtomicBool = AtomicBool::new(true);

fn unregister_mesen_listener_if_any() {
    if let Ok(mut guard) = MESEN_LISTENER.lock() {
        if *guard != 0 {
            let ptr = *guard as *mut c_void;
            *guard = 0;
            unsafe { UnregisterNotificationCallback(ptr) };
        }
    }
}

extern "C" fn on_mesen_notification(notification_type: c_int, param: *mut c_void) {
    if is_shutting_down() || param.is_null() {
        return;
    }

    if notification_type == NOTIFICATION_REFRESH_SOFTWARE_RENDERER {
        // SNES: o SoftwareRenderer manda um frame ARGB já convertido.
        let frame = unsafe { &*(param as *const SoftwareRendererFrameAbi) };
        let surface = &frame.frame;
        if surface.buffer.is_null() || surface.width == 0 || surface.height == 0 {
            return;
        }
        let w = surface.width as usize;
        let h = surface.height as usize;
        let pixel_count = w.saturating_mul(h);
        if pixel_count == 0 {
            return;
        }
        let src = unsafe { std::slice::from_raw_parts(surface.buffer, pixel_count) };

        if let Ok(mut guard) = LATEST_MESEN_FRAME.try_lock() {
            let needed_bytes = pixel_count * 4;
            if guard.rgba.len() != needed_bytes {
                guard.rgba.resize(needed_bytes, 0);
            }
            for (i, &argb) in src.iter().enumerate() {
                let base = i * 4;
                guard.rgba[base] = ((argb >> 16) & 0xFF) as u8;
                guard.rgba[base + 1] = ((argb >> 8) & 0xFF) as u8;
                guard.rgba[base + 2] = (argb & 0xFF) as u8;
                guard.rgba[base + 3] = 0xFF;
            }
            guard.width = w as u32;
            guard.height = h as u32;
            guard.has_frame = true;
        }
        return;
    }

    if notification_type != NOTIFICATION_PPU_FRAME_DONE
        || !ACTIVE_CONSOLE_IS_NES.load(Ordering::Relaxed)
    {
        return;
    }

    // NES: o PpuFrameDone passa um buffer de índices de paleta de 16 bits.
    let w = MESEN_FRAME_WIDTH.load(Ordering::Relaxed) as usize;
    let h = MESEN_FRAME_HEIGHT.load(Ordering::Relaxed) as usize;
    if w == 0 || h == 0 {
        return;
    }
    let pixel_count = w.saturating_mul(h);
    if pixel_count == 0 {
        return;
    }

    let src_slice = unsafe { std::slice::from_raw_parts(param as *const u16, pixel_count) };

    if let Ok(mut guard) = LATEST_MESEN_FRAME.try_lock() {
        let needed_bytes = pixel_count * 4;
        if guard.rgba.len() != needed_bytes {
            guard.rgba.resize(needed_bytes, 0);
        }

        for (i, &raw_px) in src_slice.iter().enumerate() {
            let rgb = NES_PALETTE_64[(raw_px & 0x3F) as usize];
            let base = i * 4;
            guard.rgba[base] = ((rgb >> 16) & 0xFF) as u8;
            guard.rgba[base + 1] = ((rgb >> 8) & 0xFF) as u8;
            guard.rgba[base + 2] = (rgb & 0xFF) as u8;
            guard.rgba[base + 3] = 0xFF;
        }
        guard.width = w as u32;
        guard.height = h as u32;
        guard.has_frame = true;
    }
}

struct MesenDesktopState {
    temp_rom_path: Mutex<Option<PathBuf>>,
}

impl Default for MesenDesktopState {
    fn default() -> Self {
        Self {
            temp_rom_path: Mutex::new(None),
        }
    }
}

impl Drop for MesenDesktopState {
    fn drop(&mut self) {
        mark_shutting_down();

        if let Ok(mut guard) = self.temp_rom_path.lock() {
            if let Some(path) = guard.take() {
                let _ = fs::remove_file(path);
            }
        }
    }
}

#[derive(Serialize)]
struct MesenInitResult {
    width: u32,
    height: u32,
}

#[derive(Serialize)]
struct MesenFrameResult {
    frame: Vec<u8>,
    width: u32,
    height: u32,
    #[serde(rename = "processingTimeMs")]
    processing_time_ms: f64,
}

fn ensure_mesen_initialized() {
    if is_shutting_down() {
        return;
    }

    MESEN_INIT_ONCE.call_once(|| {
        if is_shutting_down() {
            return;
        }
        let home = CString::new("/tmp/mesen_home").expect("CString inválido");
        // O MesenCE só instala os callbacks de áudio/vídeo quando os handles
        // são não-nulos. Um ponteiro "dangling" é suficiente: usamos software
        // renderer e os frames vêm do RegisterNotificationCallback.
        //
        // Atenção: no MesenCE `noVideo` controla o áudio e `noAudio` controla
        // o renderer (flags trocadas em EmuApiWrapper.cpp). Por isso ambos
        // devem ser `false` para o som funcionar.
        let handle = std::ptr::NonNull::<u8>::dangling()
            .as_ptr()
            .cast::<c_void>();
        unsafe {
            InitializeEmu(home.as_ptr(), handle, handle, true, false, false, true);
        }
        // O Mesen lê `gba_bios.bin` durante `LoadRom`; sem ele os jogos ficam
        // presos no vetor de SWI e a tela permanece branca.
        let _ = install_gba_bios(home.to_str().unwrap_or("/tmp/mesen_home"));
        unsafe {
            InitDll();
            configure_audio();
            let listener = RegisterNotificationCallback(Some(on_mesen_notification));
            if let Ok(mut guard) = MESEN_LISTENER.lock() {
                *guard = listener as usize;
            }
        }
    });
}

fn apply_button_to_state(pad: &mut DebugControllerState, button: &str, pressed: bool) {
    match button {
        "A" => pad.a = pressed,
        "B" => pad.b = pressed,
        "X" => pad.x = pressed,
        "Y" => pad.y = pressed,
        "L" => pad.l = pressed,
        "R" => pad.r = pressed,
        "U" => pad.u = pressed,
        "D" => pad.d = pressed,
        "Up" => pad.up = pressed,
        "Down" => pad.down = pressed,
        "Left" => pad.left = pressed,
        "Right" => pad.right = pressed,
        "Select" => pad.select = pressed,
        "Start" => pad.start = pressed,
        _ => {}
    }
}

#[tauri::command]
fn mesen_init_rom(
    rom_bytes: Vec<u8>,
    console: Option<String>,
    state: State<'_, MesenDesktopState>,
) -> Result<MesenInitResult, String> {
    if is_shutting_down() {
        return Err("Aplicação em processo de encerramento.".into());
    }
    if rom_bytes.is_empty() {
        return Err("ROM vazia.".into());
    }

    let console_name = console.as_deref().unwrap_or("NES");
    let is_nes = console_name.eq_ignore_ascii_case("NES");
    let is_gba = console_name.eq_ignore_ascii_case("GBA");
    let is_gb = console_name.eq_ignore_ascii_case("GB");
    let is_gbc = console_name.eq_ignore_ascii_case("GBC");
    let is_gameboy = is_gb || is_gbc;
    let is_pce = console_name.eq_ignore_ascii_case("PCE");
    ACTIVE_CONSOLE_IS_NES.store(is_nes, Ordering::Relaxed);

    ensure_mesen_initialized();

    if let Ok(mut guard) = LATEST_MESEN_FRAME.lock() {
        guard.has_frame = false;
    }

    if let Ok(mut pads) = ACTIVE_CONTROLLERS.lock() {
        pads[0] = empty_pad_state();
        pads[1] = empty_pad_state();
    }

    // O Mesen detecta o console pela extensão do arquivo.
    let extension = if is_nes {
        "nes"
    } else if is_gba {
        "gba"
    } else if is_gb {
        // Mesen detecta o console pela extensão; `.gb` força o DMG clássico.
        "gb"
    } else if is_gbc {
        "gbc"
    } else if is_pce {
        "pce"
    } else {
        "sfc"
    };
    let rom_file =
        std::env::temp_dir().join(format!("emmu_mesen_rom_{}.{extension}", std::process::id()));
    fs::write(&rom_file, &rom_bytes).map_err(|e| format!("Falha ao gravar ROM temporária: {e}"))?;

    if let Ok(mut guard) = state.temp_rom_path.lock() {
        *guard = Some(rom_file.clone());
    }

    let rom_str = rom_file.to_string_lossy().to_string();
    let rom_cstr = CString::new(rom_str.as_str()).map_err(|e| format!("CString inválido: {e}"))?;

    if is_shutting_down() {
        return Err("Aplicação em processo de encerramento.".into());
    }

    // P1 e P2 sempre conectados. A preferência do Player é aplicada no JS
    // (teclas P2 ignoradas quando desabilitado), sem mexer na config do Mesen
    // em runtime — o que exigiria reconstruir os control devices.
    if is_nes {
        connect_nes_controllers();
    } else if is_gba {
        connect_gba_controllers();
    } else if is_gb {
        connect_gameboy_dmg_controllers();
    } else if is_gbc {
        connect_gameboy_controllers();
    } else if is_pce {
        connect_pce_controllers();
    } else {
        connect_snes_controllers();
    }

    let loaded = unsafe { LoadRom(rom_cstr.as_ptr().cast_mut(), std::ptr::null_mut()) };
    if !loaded {
        return Err("Mesen falhou ao carregar a ROM.".into());
    }

    // O override de controle do Mesen só é aplicado quando o debugger existe.
    initialize_debugger();
    set_input_overrides(0, empty_pad_state());
    set_input_overrides(1, empty_pad_state());

    // Não chamar `GetBaseScreenSize()` aqui: ele entra em
    // `VideoDecoder::GetBaseFrameInfo()`, que usando `_emu`/`_videoFilter`
    // já liberados no shutdown causa EXC_BAD_ACCESS. As dimensões são fixas
    // e o callback de notificação já publica o tamanho real de cada frame.
    let (width, height) = if is_nes {
        (NES_WIDTH, NES_FRAME_HEIGHT)
    } else if is_gba {
        (GBA_WIDTH, GBA_FRAME_HEIGHT)
    } else if is_gameboy {
        (GAMEBOY_WIDTH, GAMEBOY_FRAME_HEIGHT)
    } else if is_pce {
        (PCE_WIDTH, PCE_FRAME_HEIGHT)
    } else {
        (NES_WIDTH, SNES_FRAME_HEIGHT)
    };
    MESEN_FRAME_WIDTH.store(width as u32, Ordering::Relaxed);
    MESEN_FRAME_HEIGHT.store(height as u32, Ordering::Relaxed);

    Ok(MesenInitResult {
        width: width as u32,
        height: height as u32,
    })
}

#[tauri::command]
fn mesen_run_frame(render_graphics: Option<bool>) -> Result<MesenFrameResult, String> {
    let start = std::time::Instant::now();
    let should_render = render_graphics.unwrap_or(true);

    let guard = LATEST_MESEN_FRAME
        .lock()
        .map_err(|_| "Falha ao acessar buffer de frame do Mesen.".to_string())?;

    let width = guard.width.max(1);
    let height = guard.height.max(1);
    let frame = if !is_shutting_down() && should_render && guard.has_frame {
        guard.rgba.clone()
    } else {
        Vec::new()
    };

    Ok(MesenFrameResult {
        frame,
        width,
        height,
        processing_time_ms: start.elapsed().as_secs_f64() * 1000.0,
    })
}

#[tauri::command]
fn mesen_set_button(button: String, pressed: bool, player: Option<u32>) -> Result<(), String> {
    if is_shutting_down() {
        return Ok(());
    }

    let slot = if player.unwrap_or(1) == 2 {
        1usize
    } else {
        0usize
    };

    let mut pads = ACTIVE_CONTROLLERS
        .lock()
        .map_err(|_| "Falha ao acessar estado dos controles.".to_string())?;

    apply_button_to_state(&mut pads[slot], button.as_str(), pressed);
    set_input_overrides(slot as u32, pads[slot]);

    Ok(())
}

#[tauri::command]
fn mesen_reset() -> Result<(), String> {
    if is_shutting_down() {
        return Ok(());
    }
    if let Ok(mut guard) = LATEST_MESEN_FRAME.lock() {
        guard.has_frame = false;
    }
    reset();
    Ok(())
}

#[tauri::command]
fn mesen_stop() -> Result<(), String> {
    if let Ok(mut pads) = ACTIVE_CONTROLLERS.lock() {
        pads[0] = empty_pad_state();
        pads[1] = empty_pad_state();
    }
    if !is_shutting_down() {
        set_input_overrides(0, empty_pad_state());
        set_input_overrides(1, empty_pad_state());
        unsafe {
            if IsRunning() {
                Stop();
            }
        }
    }
    if let Ok(mut guard) = LATEST_MESEN_FRAME.lock() {
        guard.has_frame = false;
    }
    Ok(())
}

fn release_mesen_once() {
    if MESEN_RELEASED.swap(true, Ordering::SeqCst) {
        return;
    }
    if !is_shutting_down() {
        set_input_overrides(0, empty_pad_state());
        set_input_overrides(1, empty_pad_state());
    }
    mark_shutting_down();
    unregister_mesen_listener_if_any();

    if let Ok(mut pads) = ACTIVE_CONTROLLERS.lock() {
        pads[0] = empty_pad_state();
        pads[1] = empty_pad_state();
    }
    if let Ok(mut guard) = LATEST_MESEN_FRAME.lock() {
        guard.has_frame = false;
    }

    unsafe {
        if IsRunning() {
            Stop();
        }
        Release();
    }
}

#[tauri::command]
fn mesen_release() -> Result<(), String> {
    release_mesen_once();
    Ok(())
}

fn mesen_state_path() -> PathBuf {
    std::env::temp_dir().join(format!("emmu_mesen_state_{}.mss", std::process::id()))
}

#[tauri::command]
fn mesen_save_state() -> Result<Vec<u8>, String> {
    if is_shutting_down() {
        return Err("Aplicação em processo de encerramento.".into());
    }
    let path = mesen_state_path();
    let path_str = path.to_string_lossy().to_string();
    save_state_to_file(&path_str).map_err(|e| format!("Falha ao salvar estado: {e}"))?;
    let bytes = fs::read(&path).map_err(|e| format!("Falha ao ler estado: {e}"))?;
    let _ = fs::remove_file(&path);
    Ok(bytes)
}

#[tauri::command]
fn mesen_load_state(state: Vec<u8>) -> Result<(), String> {
    if is_shutting_down() {
        return Ok(());
    }
    if state.is_empty() {
        return Err("Estado salvo vazio.".into());
    }
    let path = mesen_state_path();
    fs::write(&path, &state).map_err(|e| format!("Falha ao gravar estado: {e}"))?;
    let path_str = path.to_string_lossy().to_string();
    load_state_from_file(&path_str).map_err(|e| format!("Falha ao carregar estado: {e}"))?;
    let _ = fs::remove_file(&path);
    Ok(())
}

/// "Desconectar" o P2 no Player: limpa os overrides para que nenhum botão
/// fique preso. A porta continua conectada no Mesen para que reabilitar o P2
/// funcione imediatamente, sem recarregar a ROM.
#[tauri::command]
fn mesen_set_p2_enabled(enabled: bool) -> Result<(), String> {
    if enabled {
        return Ok(());
    }
    if let Ok(mut pads) = ACTIVE_CONTROLLERS.lock() {
        pads[1] = empty_pad_state();
    }
    if !is_shutting_down() {
        set_input_overrides(1, empty_pad_state());
    }
    Ok(())
}

#[tauri::command]
fn mesen_set_volume(volume: u32) -> Result<(), String> {
    set_audio_volume(volume);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .manage(MesenDesktopState::default())
        .invoke_handler(tauri::generate_handler![
            mesen_init_rom,
            mesen_run_frame,
            mesen_set_button,
            mesen_reset,
            mesen_stop,
            mesen_release,
            mesen_save_state,
            mesen_load_state,
            mesen_set_volume,
            mesen_set_p2_enabled,
        ]);

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
                let _ = app.emit(event_name, ());
            }
        });

    let app = builder
        .build(tauri::generate_context!())
        .expect("failed to build Emmu application");

    app.run(|_app_handle, event| {
        if matches!(
            event,
            tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit
        ) {
            // O JS pode não ter tempo de chamar `mesen_release` quando a
            // janela fecha. Encerra o Mesen aqui para impedir que a thread de
            // render acesse o VideoDecoder já destruído (EXC_BAD_ACCESS).
            release_mesen_once();
        }
    });
}
