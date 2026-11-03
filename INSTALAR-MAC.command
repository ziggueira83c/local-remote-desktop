#!/bin/bash
# ============================================================
#  Instalador em 1 clique do Local Remote Desktop (macOS)
#  1. Instala o que faltar: Ferramentas de Linha de Comando do
#     Xcode, Rust e Node.js
#  2. Compila o programa
#  3. Copia para /Aplicativos e abre os ajustes de permissão
# ============================================================

cd "$(dirname "$0")" || exit 1
ROOT="$(pwd)"
LOG="$ROOT/compilacao-log.txt"

pausa() { echo; read -r -p "Pressione Enter para fechar..." _; }
falha() {
  echo
  echo "============================================================"
  echo "  A INSTALAÇÃO NÃO TERMINOU: $1"
  echo "============================================================"
  echo " O registro está em: $LOG"
  echo " Envie esse arquivo para o Claude corrigir o problema."
  pausa
  exit 1
}

if [ -f "$ROOT/apps/desktop/package.json" ]; then
  PROJ="$ROOT"
elif [ -f "$ROOT/local-remote-desktop/apps/desktop/package.json" ]; then
  PROJ="$ROOT/local-remote-desktop"
else
  echo "Não encontrei o código do programa. Deixe este arquivo na pasta que tem 'apps' e 'crates'."
  pausa; exit 1
fi

clear
echo "============================================================"
echo "  LOCAL REMOTE DESKTOP - INSTALAÇÃO NO MAC"
echo "============================================================"
echo " Na primeira vez pode levar de 15 a 30 minutos."
echo " O Mac vai pedir sua senha em alguns momentos."
echo " Não feche esta janela."
echo "============================================================"
echo
: > "$LOG"

# --- 1. Ferramentas de Linha de Comando do Xcode ---
echo "[1/5] Verificando Ferramentas de Linha de Comando do Xcode..."
if ! xcode-select -p >/dev/null 2>&1; then
  echo "      Abrindo o instalador da Apple. Clique em 'Instalar' na janela que aparecer."
  xcode-select --install >/dev/null 2>&1
  until xcode-select -p >/dev/null 2>&1; do sleep 10; done
fi
echo "      OK."

# --- 2. Rust ---
echo "[2/5] Verificando Rust..."
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
if ! command -v cargo >/dev/null 2>&1; then
  echo "      Instalando Rust..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y >>"$LOG" 2>&1 || falha "não foi possível instalar o Rust"
  source "$HOME/.cargo/env"
fi
rustup default stable >>"$LOG" 2>&1
command -v cargo >/dev/null 2>&1 || falha "o Rust não ficou disponível"
echo "      OK."

# --- 3. Node.js ---
echo "[3/5] Verificando Node.js..."
export PATH="/usr/local/bin:/opt/homebrew/bin:$PATH"
if ! command -v npm >/dev/null 2>&1; then
  echo "      Baixando Node.js LTS (vai pedir sua senha para instalar)..."
  PKG=$(curl -fsSL https://nodejs.org/dist/latest-v22.x/ | grep -o 'node-v22[0-9.]*\.pkg' | head -1)
  [ -n "$PKG" ] || falha "não foi possível encontrar o instalador do Node.js"
  curl -fsSL -o /tmp/node-lts.pkg "https://nodejs.org/dist/latest-v22.x/$PKG" || falha "download do Node.js falhou"
  sudo installer -pkg /tmp/node-lts.pkg -target / >>"$LOG" 2>&1 || falha "instalação do Node.js falhou"
  export PATH="/usr/local/bin:$PATH"
fi
command -v npm >/dev/null 2>&1 || falha "o Node.js não ficou disponível"
echo "      OK."

# --- 4. Compilação ---
echo "[4/5] Compilando o programa (parte mais demorada)..."
cd "$PROJ/apps/desktop" || falha "pasta apps/desktop não encontrada"
# Pasta node_modules copiada do Windows não serve no Mac.
if [ -d node_modules ] && ! ls node_modules/@tauri-apps 2>/dev/null | grep -q "cli-darwin"; then
  rm -rf node_modules
fi
npm install --no-fund --no-audit 2>&1 | tee -a "$LOG"
[ "${PIPESTATUS[0]}" -eq 0 ] || falha "npm install falhou"
npm run tauri build -- --bundles app 2>&1 | tee -a "$LOG"
[ "${PIPESTATUS[0]}" -eq 0 ] || falha "erro de compilação"

APP=$(ls -d "$PROJ"/target/release/bundle/macos/*.app 2>/dev/null | head -1)
[ -n "$APP" ] || falha "a compilação terminou, mas o app não foi encontrado"

# --- 5. Instalação ---
echo "[5/5] Instalando em /Aplicativos..."
DEST="/Applications/$(basename "$APP")"
if ! (rm -rf "$DEST" && cp -R "$APP" /Applications/) 2>/dev/null; then
  sudo rm -rf "$DEST" && sudo cp -R "$APP" /Applications/ || falha "não foi possível copiar para /Aplicativos"
fi
# Assinatura local (ad-hoc) para o macOS reconhecer o app nas permissões.
codesign --force --deep --sign - "$DEST" >>"$LOG" 2>&1
xattr -dr com.apple.quarantine "$DEST" 2>/dev/null

open "$DEST"
open "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
sleep 1
open "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"

echo
echo "============================================================"
echo "  PRONTO! O Local Remote Desktop está em /Aplicativos."
echo "============================================================"
echo " Para este Mac poder ser ACESSADO, ative o app nos ajustes que abriram:"
echo "   - Privacidade e Segurança > Gravação de Tela (ou Gravação de Tela e Áudio do Sistema)"
echo "   - Privacidade e Segurança > Acessibilidade"
echo " Se o app não aparecer na lista, clique em '+' e escolha-o em Aplicativos."
echo " Depois FECHE e ABRA o app de novo."
echo " Se o Mac perguntar sobre acesso à Rede Local, clique em Permitir."
pausa
