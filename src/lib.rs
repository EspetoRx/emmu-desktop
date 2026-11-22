/** emmu/platforms/desktop/src/lib.rs */
use emmu_mesen_sys::{
    DebugControllerState, InitDll, InitializeEmu, IsRunning, LoadRom, NOTIFICATION_PPU_FRAME_DONE,
    NOTIFICATION_REFRESH_SOFTWARE_RENDERER, RegisterNotificationCallback, Release,
    SoftwareRendererFrameAbi, Stop, UnregisterNotificationCallback, configure_audio,
    connect_gameboy_controllers, connect_gameboy_dmg_controllers, connect_gba_controllers,
    connect_nes_controllers, connect_pce_controllers, connect_sms_controllers,
    connect_snes_controllers, connect_ws_controllers, initialize_debugger, install_gba_bios,
    is_shutting_down, load_state_from_file, mark_shutting_down, reset, save_state_to_file,
    set_audio_volume, set_input_overrides,
};
use serde::Serialize;
use std::ffi::CString;
use std::fs;
use std::os::raw::{c_int, c_void};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, Once};
use tauri::State;
use tauri::http::{Response as HttpResponse, StatusCode};
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
/// Game Gear usa a mesma área útil do LCD do Game Boy após o overscan do Mesen.
const GAMEBOY_WIDTH: usize = 160;
const GAMEBOY_FRAME_HEIGHT: usize = 144;
/// Master System publica o frame NTSC de 256x224 (o callback de notificação
/// pode atualizar para 256x240 em ROMs PAL).
const SMS_WIDTH: usize = 256;
const SMS_FRAME_HEIGHT: usize = 224;
/// Mesen publica o frame PCE em 256x242 (o mesmo conteúdo do core WASM, sem o
/// overscan horizontal esquerdo/direito). O callback de notificação informa
/// as dimensões reais, então estes valores são apenas o ponto de partida do
/// canvas antes do primeiro frame.
const PCE_WIDTH: usize = 256;
const PCE_FRAME_HEIGHT: usize = 242;
/// WonderSwan publica o frame LCD nativo em 224x144 (sem ícones e sem rotação).
const WS_WIDTH: usize = 224;
const WS_FRAME_HEIGHT: usize = 144;

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

/// Codifica um frame RGBA8888 como BMP de 24 bits (bottom-up, BGR) para ser
/// servido ao WebView via protocolo customizado. O navegador decodifica o BMP
/// nativamente, fora da thread principal, sem trazer megabytes de pixels para
/// o JavaScript — diferente de `Vec<u8>` em `invoke`, que vira um array JSON.
fn frame_as_bmp(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let row_size = w * 3;
    let padded_row_size = (row_size + 3) & !3;
    let pixel_bytes = padded_row_size * h;
    let total = 54 + pixel_bytes;
    let mut bmp = vec![0u8; total];

    bmp[0..2].copy_from_slice(b"BM");
    bmp[2..6].copy_from_slice(&(total as u32).to_le_bytes());
    bmp[10..14].copy_from_slice(&54u32.to_le_bytes());

    // BITMAPINFOHEADER
    bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
    bmp[18..22].copy_from_slice(&(width as i32).to_le_bytes());
    bmp[22..26].copy_from_slice(&(height as i32).to_le_bytes());
    bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
    bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
    bmp[34..38].copy_from_slice(&(pixel_bytes as u32).to_le_bytes());

    for row in 0..h {
        // BMP é bottom-up: a primeira linha do arquivo é a última do frame.
        let src_y = h - 1 - row;
        let dst_base = 54 + row * padded_row_size;
        for col in 0..w {
            let src = (src_y * w + col) * 4;
            if src + 2 >= rgba.len() {
                break;
            }
            let dst = dst_base + col * 3;
            bmp[dst] = rgba[src + 2]; // B
            bmp[dst + 1] = rgba[src + 1]; // G
            bmp[dst + 2] = rgba[src]; // R
        }
    }
    bmp
}

/// Codifica o último frame como PNG para a miniatura da biblioteca. Isso é
/// feito no Rust porque o frame exibido vem de `emmu-frame://` (outra origem),
/// o que deixa o `<canvas>` do WebView marcado como "tainted" e faz
/// `canvas.toBlob()` falhar silenciosamente no desktop.
fn frame_as_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let expected = width as usize * height as usize * 4;
    if width == 0 || height == 0 || rgba.len() < expected {
        return Err("Frame do Mesen indisponível para captura.".to_owned());
    }

    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|error| format!("Falha ao iniciar PNG: {error}"))?;
        writer
            .write_image_data(&rgba[..expected])
            .map_err(|error| format!("Falha a escrever PNG: {error}"))?;
    }
    Ok(png_bytes)
}

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

        if let Ok(mut guard) = LATEST_MESEN_FRAME.lock() {
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

    if let Ok(mut guard) = LATEST_MESEN_FRAME.lock() {
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
    width: u32,
    height: u32,
    has_frame: bool,
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
    let is_gg = console_name.eq_ignore_ascii_case("GG");
    let is_sms = console_name.eq_ignore_ascii_case("SMS");
    let is_ws = console_name.eq_ignore_ascii_case("WS");
    let is_pce = console_name.eq_ignore_ascii_case("PCE");
    let is_n64 = console_name.eq_ignore_ascii_case("N64");
    let is_nds = console_name.eq_ignore_ascii_case("NDS");
    let is_psp = console_name.eq_ignore_ascii_case("PSP");
    if is_n64 || is_nds || is_psp {
        let label = if is_psp {
            "PlayStation Portable"
        } else if is_nds {
            "Nintendo DS"
        } else {
            "Nintendo 64"
        };
        return Err(format!(
            "{} não é suportado pelo backend Mesen; o Emmu executa este console em WASM dentro do WebView.",
            label
        ));
    }
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
    } else if is_gg {
        // O Mesen detecta SMS/Game Gear pela extensão; `.gg` seleciona o
        // modelo Game Gear e a região de LCD correta.
        "gg"
    } else if is_sms {
        // `.sms` seleciona o Master System no Mesen.
        "sms"
    } else if is_ws {
        // `.ws` deixa o Mesen escolher Mono/Color pelo footer da ROM.
        "ws"
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
    } else if is_gg || is_sms {
        connect_sms_controllers();
    } else if is_ws {
        connect_ws_controllers();
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
    } else if is_gameboy || is_gg {
        (GAMEBOY_WIDTH, GAMEBOY_FRAME_HEIGHT)
    } else if is_sms {
        (SMS_WIDTH, SMS_FRAME_HEIGHT)
    } else if is_ws {
        (WS_WIDTH, WS_FRAME_HEIGHT)
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
    let has_frame = !is_shutting_down() && should_render && guard.has_frame;

    // Não serializar os pixels aqui: o frontend busca o frame como BMP pelo
    // protocolo `emmu-frame`. Um `Vec<u8>` nesta resposta se torna um array
    // JSON de ~800 KB por frame e trava a thread principal do WebView.
    Ok(MesenFrameResult {
        width,
        height,
        has_frame,
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
fn mesen_capture_preview() -> Result<Vec<u8>, String> {
    if is_shutting_down() {
        return Err("Aplicação em processo de encerramento.".into());
    }
    let (rgba, width, height) = {
        let guard = LATEST_MESEN_FRAME
            .lock()
            .map_err(|_| "Falha ao acessar buffer de frame do Mesen.".to_string())?;
        if !guard.has_frame || guard.rgba.is_empty() {
            return Err("Ainda não há um frame do jogo para capturar.".to_owned());
        }
        (guard.rgba.clone(), guard.width, guard.height)
    };
    frame_as_png(&rgba, width, height)
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
        // Servir o frame como imagem BMP evita serializar ~245 KB de pixels
        // como array JSON a 60 fps pelo `invoke`, que saturava a thread do
        // WebView e impedia a imagem de aparecer (o áudio vinha do SDL).
        .register_asynchronous_uri_scheme_protocol("emmu-frame", |_app, _request, responder| {
            let (rgba, width, height) = {
                match LATEST_MESEN_FRAME.lock() {
                    Ok(guard) if guard.has_frame && !guard.rgba.is_empty() => {
                        (guard.rgba.clone(), guard.width, guard.height)
                    }
                    _ => (Vec::new(), NES_WIDTH as u32, NES_FRAME_HEIGHT as u32),
                }
            };
            let body = if rgba.is_empty() {
                frame_as_bmp(
                    &vec![0u8; width as usize * height as usize * 4],
                    width,
                    height,
                )
            } else {
                frame_as_bmp(&rgba, width, height)
            };
            responder.respond(
                HttpResponse::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "image/bmp")
                    .header("Cache-Control", "no-store")
                    .body(body)
                    .expect("resposta BMP inválida"),
            );
        })
        .invoke_handler(tauri::generate_handler![
            mesen_init_rom,
            mesen_run_frame,
            mesen_set_button,
            mesen_reset,
            mesen_stop,
            mesen_release,
            mesen_save_state,
            mesen_load_state,
            mesen_capture_preview,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_as_bmp_has_valid_header_and_size() {
        let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255];
        let bmp = frame_as_bmp(&rgba, 2, 1);
        assert_eq!(&bmp[0..2], b"BM");
        let pixel_offset = u32::from_le_bytes(bmp[10..14].try_into().unwrap()) as usize;
        assert_eq!(pixel_offset, 54);
        assert_eq!(bmp.len(), 54 + 8);
        // BMP é BGR: vermelho (255,0,0) vira [0,0,255], verde vira [0,255,0].
        // Cada linha é alinhada a 4 bytes, então 2 pixels ocupam 8 bytes.
        assert_eq!(&bmp[54..57], &[0, 0, 255]);
        assert_eq!(&bmp[57..60], &[0, 255, 0]);
    }

    #[test]
    fn frame_as_png_has_valid_header_and_size() {
        let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255];
        let png = frame_as_png(&rgba, 2, 1).expect("PNG deve ser gerado");
        assert_eq!(&png[0..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
        assert_eq!((width, height), (2, 1));
        assert!(png.len() > 33);
        assert!(frame_as_png(&rgba, 0, 1).is_err());
    }
}
