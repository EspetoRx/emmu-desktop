#!/usr/bin/env bash
# Prepara TODAS as dependências de runtime que acompanham o instalador desktop.
#
# Consoles Mesen (NES/SNES/GBA/GB/GBC/GG/SMS/WS/PCE):
#   MesenCore.dylib/.so + libSDL2-2.0.0.dylib/.so.0 + avisos de licença.
#   São copiados para crates/emmu-mesen-sys/runtime/ e declarados em
#   tauri.conf.json -> bundle.resources.
#
# Nintendo 64:
#   n64wasm.js, n64wasm.wasm e assets.zip são buscados via fetch pelo bridge
#   dentro do WebView. Eles entram no instalador de duas formas: embutidos no
#   frontendDist (dist/wasm/n64/) e como resources explícitos em tauri.conf.json.
#
# Nintendo DS:
#   o módulo WASM do dust é importado pelo worker dentro do frontendDist; não
#   há assets soltos. Apenas a licença GPL é copiada para runtime/.
#
# PlayStation Portable:
#   ppsspp_libretro.js/.wasm, ppsspp-assets.zip e license.txt são servidos pelo
#   WebView a partir de dist/wasm/psp/ e também declarados em bundle.resources.
#   A licença GPL-2.0 do PPSSPP é copiada para runtime/ com os demais avisos.
#
# Uso:
#   bash platforms/desktop/scripts/prepare-bundle.sh
#   npm run desktop:build   # chama este script antes do cargo tauri build
set -euo pipefail

DESKTOP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT_DIR="$(cd "$DESKTOP_DIR/../.." && pwd)"
MESEN_DIR="$ROOT_DIR/crates/emmu-mesen-sys"
RUNTIME="$MESEN_DIR/runtime"
N64_VENDOR="$ROOT_DIR/crates/emmu-n64/vendor/n64wasm"
N64_PUBLIC="$ROOT_DIR/platforms/web/ui/public/wasm/n64"
TARGET="${MESEN_TARGET:-$(rustc -vV | sed -n 's/^host: //p')}"

# Builds feitos com a configuração antiga (destino "wasm/n64/" no Tauri)
# deixaram um ARQUIVO chamado n64 em target/<perfil>/wasm/. O codegen tenta
# criar o diretório e falha com "File exists (os error 17)". Só remove quando
# é arquivo; um diretório legítimo é preservado.
for profile in debug release; do
  stale="$ROOT_DIR/target/$profile/wasm/n64"
  if [[ -f "$stale" ]]; then
    echo "==> Removendo resíduo de build antigo: $stale"
    rm -f "$stale"
  fi
done

case "$TARGET" in
  x86_64-apple-darwin|aarch64-apple-darwin)
    CORE_FILE=MesenCore.dylib
    SDL_LIB=libSDL2-2.0.0.dylib
    ;;
  x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu)
    CORE_FILE=MesenCore.so
    SDL_LIB=libSDL2-2.0.so.0
    ;;
  *)
    echo "Target sem suporte: $TARGET (use x86_64/aarch64 apple-darwin ou unknown-linux-gnu)" >&2
    echo "Não há MesenCore/SDL2 para Windows neste repositório." >&2
    exit 1
    ;;
esac

fail() {
  echo "prepare-bundle: $*" >&2
  exit 1
}

# 1. Runtime nativo (Mesen + SDL2). build-sdl2.sh é idempotente o bastante
#    para ser chamado de novo se algum arquivo estiver faltando.
if [[ ! -f "$RUNTIME/$CORE_FILE" || ! -f "$RUNTIME/$SDL_LIB" ]]; then
  echo "==> Runtime Mesen/SDL2 incompleto; gerando $TARGET"
  [[ -f "$MESEN_DIR/vendor/SDL/LICENSE.txt" ]] ||
    fail "submodule SDL ausente (git submodule update --init --recursive)"
  MESEN_TARGET="$TARGET" bash "$MESEN_DIR/scripts/build-sdl2.sh"
fi

[[ -f "$RUNTIME/$CORE_FILE" ]] || fail "$CORE_FILE não foi copiado para $RUNTIME"
[[ -f "$RUNTIME/$SDL_LIB" ]] || fail "$SDL_LIB não foi copiado para $RUNTIME"

# Corrige runtime gerado por versões antigas do build-sdl2.sh: sem @rpath no
# install name, o dyld não usa o LC_RPATH do executável e o app instalado
# fecha com "Library not loaded: MesenCore.dylib".
if [[ "$TARGET" == *apple-darwin ]]; then
  core_id="$(otool -D "$RUNTIME/$CORE_FILE" | tail -1)"
  if [[ "$core_id" != "@rpath/$CORE_FILE" ]]; then
    echo "==> Ajustando install name do MesenCore para @rpath/$CORE_FILE"
    install_name_tool -id "@rpath/$CORE_FILE" "$RUNTIME/$CORE_FILE"
  fi
fi

# 2. Assets do N64Wasm. Mesmo já embutidos em dist/ pelo Vite, cópias
#    explícitas entram no pacote via bundle.resources e servem de fallback.
for asset in n64wasm.js n64wasm.wasm assets.zip LICENSE; do
  [[ -f "$N64_VENDOR/$asset" ]] ||
    fail "asset N64Wasm ausente: $N64_VENDOR/$asset"
done

mkdir -p "$N64_PUBLIC"
for asset in n64wasm.js n64wasm.wasm assets.zip; do
  cp "$N64_VENDOR/$asset" "$N64_PUBLIC/$asset"
done

# 3. Avisos de licença que acompanham as bibliotecas de terceiros no instalador.
cp "$N64_VENDOR/LICENSE"               "$RUNTIME/LICENSE-N64WASM.txt"
cp "$ROOT_DIR/crates/emmu-gba/LICENSE-WEB-GBA-EMULATOR.txt" \
                                       "$RUNTIME/LICENSE-WEB-GBA.txt"
cp "$MESEN_DIR/LICENSE-GBA-BIOS.txt"   "$RUNTIME/LICENSE-GBA-BIOS.txt"
cp "$DESKTOP_DIR/licenses/GPL-3.0.txt" "$RUNTIME/LICENSE-GPL-3.0.txt"
cp "$ROOT_DIR/crates/emmu-nds/vendor/dust/LICENSE" "$RUNTIME/LICENSE-DUST.txt"
cp "$ROOT_DIR/crates/emmu-psp/vendor/ppsspp-ejs/license.txt" \
                                        "$RUNTIME/LICENSE-PPSSPP.txt"
cp "$DESKTOP_DIR/THIRD-PARTY-NOTICES.txt" \
                                       "$RUNTIME/THIRD-PARTY-NOTICES.txt"

# O build-sdl2.sh já copia a licença do SDL2; garanta mesmo em runtime pronto.
[[ -f "$RUNTIME/LICENSE-SDL2.txt" ]] ||
  cp "$MESEN_DIR/vendor/SDL/LICENSE.txt" "$RUNTIME/LICENSE-SDL2.txt"

cat > "$RUNTIME/LICENSE-MESEN.txt" <<'EOF'
MesenCE / Mesen 2 — GPL-3.0
Copyright (C) 2014-2025 Sour

Este produto inclui o núcleo MesenCore, distribuído sob a GNU General
Public License, versão 3. O texto integral acompanha o instalador em
LICENSE-GPL-3.0.txt e também está disponível em
https://www.gnu.org/licenses/gpl-3.0.txt . O código-fonte correspondente
em https://github.com/SourMesen/Mesen2 .
EOF

echo "==> Dependências preparadas em $RUNTIME"
ls -lh "$RUNTIME"
echo "==> Assets N64 preparados em $N64_PUBLIC"
ls -lh "$N64_PUBLIC"
