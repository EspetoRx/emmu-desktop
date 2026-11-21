# Emmu Desktop

Aplicativo **Tauri 2** do Emmu. A interface é a mesma SPA usada no navegador (`../web/ui`), mas a emulação é feita pelo MesenCE através de `crates/emmu-mesen-sys`. Isto cobre NES, SNES, GBA, GB, GBC e PCE com áudio SDL e save states `.mss`.

## Arquitetura

```text
Vue UI (../web/ui)
  │
  ├─ invoke("mesen_*") ──► src/lib.rs ──► emmu-mesen-sys ──► MesenCore.dylib/.so
  │                                                            └─ dlopen(SDL2)
  └─ emmu-frame://localhost/frame?n=<nonce> ◄── frames BMP gerados no Rust
```

- Comandos Tauri registrados: `mesen_init_rom`, `mesen_run_frame`, `mesen_set_button`, `mesen_reset`, `mesen_stop`, `mesen_release`, `mesen_save_state`, `mesen_load_state`, `mesen_capture_preview`, `mesen_set_volume` e `mesen_set_p2_enabled`.
- O ROM é copiado para um arquivo temporário com extensão adequada (`nes`, `sfc`, `gb`, `gbc`, `gba`, `pce`) e removido no `Drop`.
- Frames são servidos como **BMP de 24 bits** pelo protocolo assíncrono `emmu-frame`, evitando arrays JSON de pixels via `invoke`.
- `frame_as_png()` e `mesen_capture_preview()` geram a miniatura no Rust, pois o canvas que usa `emmu-frame://` fica “tainted” e não permite `toBlob()`.
- Notificações usadas: `PPU_FRAME_DONE` (NES com paleta de 64 cores) e `REFRESH_SOFTWARE_RENDERER` (SNES/GBA/GB/GBC/PCE).
- Save states são gravados em arquivos `.mss` temporários e devolvidos à UI como bytes.
- Volume (0–100) é repassado ao core via `set_audio_volume`.
- Menu nativo emite `open-rom-request`, `return-to-library`, `configure-controls` e `show-help`.
- `RunEvent::ExitRequested | Exit` chama `Stop()`/`Release()` para evitar `EXC_BAD_ACCESS` no encerramento.

## Resoluções iniciais

| Console | Largura × altura publicadas |
| --- | --- |
| NES | 256 × 240 |
| SNES | 256 × 224 |
| GBA | 240 × 160 |
| GB/GBC | 160 × 144 |
| PCE | 256 × 242 |

O tamanho real é atualizado pelo callback de frame. No navegador, o PCE WASM usa 282 × 242 com overscan.

## Árvore de arquivos

```text
platforms/desktop/
├── Cargo.toml                # desktop_lib, Tauri 2, png, serde, emmu-*
├── MesenCore.dylib           # binário de desenvolvimento no macOS
├── build.rs                  # tauri-build + rpath para Resources/lib
├── capabilities/
│   └── default.json          # permissão core:default para a janela main
├── gen/                      # artefatos gerados pelo Tauri (schemas, iOS, macOS)
├── icons/
│   └── icon.png              # ícone do aplicativo
├── src/
│   ├── lib.rs                # comandos, protocolo de frames, menu, shutdown (~800 linhas)
│   └── main.rs               # desktop_lib::run()
└── tauri.conf.json           # produto Emmu 0.1.0, janela 1280x820, bundle all
```

`tauri.conf.json` define:

- `beforeDevCommand`: `npm run dev -- --host 127.0.0.1`, cwd `../web/ui`, sem esperar;
- `devUrl`: `http://127.0.0.1:5173`;
- `beforeBuildCommand`: `npm run build`, cwd `../web/ui`;
- `frontendDist`: `../web/ui/dist`;
- `bundle.resources`: `../../crates/emmu-mesen-sys/runtime/*`;
- `iOS.minimumSystemVersion`: `15.0`;
- `identifier`: `com.emmu.app` (o Tauri avisa que termina em `.app`).

## Pré-requisitos e build

Antes de compilar, prepare o runtime Mesen/SDL2 conforme [`crates/emmu-mesen-sys/README.md`](../../crates/emmu-mesen-sys/README.md) e [`BUNDLE.md`](../../crates/emmu-mesen-sys/BUNDLE.md):

```sh
git submodule update --init --recursive
./crates/emmu-mesen-sys/scripts/build-sdl2.sh
```

Depois:

```sh
cd platforms/desktop
cargo tauri dev
cargo tauri build
```

Ou pela UI:

```sh
cd platforms/web/ui
npm run desktop:dev
npm run desktop:build
```

Saídas no macOS: `target/release/bundle/macos/Emmu.app` e `target/release/bundle/dmg/Emmu_0.1.0_aarch64.dmg`. O bundle inclui `MesenCore.dylib` e `libSDL2-2.0.0.dylib` com rpath `@executable_path/../Resources`.

Para rodar o binário de desenvolvimento direto, o `.vscode/launch.json` usa `DYLD_LIBRARY_PATH=/tmp/MesenCE/InteropDLL/obj.osx-arm64`.

## Testes

```sh
cargo test -p desktop
```

Inclui `frame_as_bmp_has_valid_header_and_size` e `frame_as_png_has_valid_header_and_size`.

## Limitações

- Nenhum core WASM é usado no desktop; a compatibilidade é a do MesenCE.
- Não há seletor de filtro de vídeo nesta camada.
- Não há UI touch específica para desktop; a UI compartilhada detecta dispositivos touch por User-Agent.
- Windows ainda não tem runtime Mesen/SDL empacotado; macOS e Linux são os alvos suportados hoje.

## Licença

Não há campo `license` em `Cargo.toml` nem arquivo `LICENSE` neste submódulo. O código é Emmu; a licença precisa ser formalizada. O bundle contém software de terceiros:

| Componente | Licença |
| --- | --- |
| MesenCE | GPL-3.0 — `Copyright (C) 2014-2025 Sour` |
| SDL2 2.32.10 | zlib — `Copyright (C) 1997-2025 Sam Lantinga` |
| BIOS GBA | MIT — `Copyright 2020 - 2021 DenSinH and fleroviux` |
| `png`, `serde`, `tauri` | MIT/Apache-2.0 conforme cada crate |

Preserve os textos de licença no instalador.
