# Local Remote Desktop (LRD) — MVP

Acesso e controle remoto **somente pela rede local**, para Windows 11 e macOS. Cada instalação é ao mesmo tempo controladora e controlada. Não usa nuvem, conta, servidor central nem relay.

> **Situação desta entrega:** o código foi escrito num ambiente sem acesso ao crates.io/npm, então **ainda não foi compilado nem testado**. É provável que a primeira compilação mostre alguns erros pontuais (nomes de API de bibliotecas). Copie a saída do erro e mande para correção.

---

## O que funciona neste MVP (fases 1–3 do documento)

| Recurso | Situação |
|---|---|
| Descoberta automática na LAN (mDNS `_lrd._udp.local.`) | ✅ |
| Conexão direta por IP ou nome, porta configurável (padrão UDP 47321) | ✅ |
| Canal QUIC + TLS 1.3 com autenticação mútua por certificado | ✅ |
| Identidade persistente (UUID + certificado; chave privada no Credential Manager / Keychain) | ✅ |
| Fingerprint SHA-256 visível + **código de verificação de 6 dígitos** nos dois lados | ✅ |
| Detecção de troca de identidade (TOFU) nos dois sentidos | ✅ |
| Pedido de acesso com Aceitar/Recusar, escopo escolhido e expiração em 60 s | ✅ |
| Perfis: Somente visualizar / Controle remoto (aplicados no computador controlado) | ✅ |
| Transmissão da tela com envio só das regiões alteradas | ✅ |
| Mouse (mover, clicar, arrastar, rolar) e teclado por **tecla física** (ABNT2/US corretos) | ✅ |
| Troca automática Ctrl ⇄ Cmd entre Windows e Mac | ✅ |
| Vários monitores, qualidade (Fluidez/Equilibrado/Nitidez) e 15/30/60 FPS | ✅ |
| Faixa sempre visível no computador acessado, com "Bloquear controle" e "Encerrar acesso" | ✅ |
| Liberação de teclas/botões presos ao desconectar | ✅ |
| Histórico de sessões, dispositivos conhecidos, favoritos, auditoria em SQLite | ✅ |
| Recusa de conexões vindas de fora da rede local + limite de tentativas por IP | ✅ |
| Métricas da sessão (FPS, Mbps, latência) | ✅ |

### Ainda não incluído (fases 4–6)

Acesso não supervisionado, reconexão automática, transferência de arquivos, clipboard, áudio, Wake-on-LAN, tela de login/UAC, instaladores assinados.

### Diferenças em relação ao documento de especificação

- **Vídeo em tiles JPEG em vez de H.264.** A tela é dividida em blocos de 128 px e só os blocos alterados são enviados. Isso dá ótima nitidez para texto e usa pouca banda no uso normal de desktop, e evita dependências nativas de codec que eu não conseguiria validar aqui. A troca para H.264 por hardware (Media Foundation / VideoToolbox) é a próxima evolução natural, e o protocolo já negocia o codec no `Hello`.
- **Captura via biblioteca `xcap`** (GDI no Windows, CoreGraphics no macOS) em vez de DXGI Desktop Duplication / ScreenCaptureKit diretos. Na prática fica entre 15 e 30 FPS em 1080p, dependendo da máquina.
- **Mensagens Protobuf declaradas com `prost` derive.** O formato de fio é Protobuf padrão (contrato em `proto/lrd.proto`), mas a compilação não exige `protoc`.
- **Um processo só** (sem serviço do Windows nem agente separado), suficiente enquanto não há acesso pré-login.

---

## Compilar e rodar

### Windows 11

1. Instale o **Visual Studio Build Tools 2022** com a carga de trabalho *Desenvolvimento para desktop com C++*: <https://visualstudio.microsoft.com/visual-cpp-build-tools/>
2. Instale o **Rust**: <https://rustup.rs> (escolha a opção padrão, toolchain MSVC).
3. Instale o **Node.js LTS**: <https://nodejs.org>
4. O WebView2 já vem no Windows 11.

### macOS (12.3 ou mais novo, Intel ou Apple Silicon)

```bash
xcode-select --install                                           # ferramentas de linha de comando
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # Rust
brew install node                                                # ou o instalador de nodejs.org
```

### Nos dois sistemas

```bash
cd local-remote-desktop/apps/desktop
npm install
npm run tauri dev        # abre o app em modo desenvolvimento
```

A primeira compilação demora (de 5 a 15 minutos). As seguintes são rápidas.

Para gerar o instalador:

```bash
npm run tauri build
```

Os arquivos ficam em `local-remote-desktop/target/release/bundle/`:
- Windows: `msi/*.msi` e `nsis/*-setup.exe`
- macOS: `macos/Local Remote Desktop.app` e `dmg/*.dmg`

Os instaladores não são assinados. No Windows, o SmartScreen pode pedir "Mais informações → Executar assim mesmo". No Mac, um app compilado na própria máquina abre normalmente. Copiado para outro Mac, ele precisa de clique direito → Abrir na primeira vez.

### Testes automáticos do núcleo

```bash
cd local-remote-desktop
cargo test --workspace --exclude lrd-desktop
```

---

## Primeiro uso

### Windows
- Na primeira execução, o Windows pergunta se o app pode usar a rede. Marque **Redes privadas** e permita.
- Se a rede estiver configurada como *Pública*, mude para *Privada* (Configurações › Rede › propriedades da conexão), senão o firewall bloqueia.
- Não é possível controlar janelas abertas "como administrador" nem a tela do UAC, por uma proteção do Windows (UIPI).

### macOS
O Mac precisa de duas permissões para ser **acessado** (para apenas controlar outro computador, nenhuma é necessária):
- **Gravação de Tela**, para enviar a imagem. O app mostra um botão "Conceder" no cartão *Este computador*.
- **Acessibilidade**, para aceitar mouse e teclado. Use o botão "Abrir ajustes" e ative o app.

Depois de conceder, **feche e abra o app de novo**.

Em modo `npm run tauri dev`, o macOS atribui as permissões ao **Terminal** (ou iTerm/VS Code), não ao app. Conceda para o programa onde você rodou o comando.

A cada nova compilação de um app não assinado, o macOS pode "esquecer" as permissões. Se a tela ficar preta ou o controle parar, remova e adicione o app de novo em Ajustes › Privacidade.

No macOS 15+, permita o acesso à **Rede Local** quando o sistema pedir.

---

## Como usar

1. Abra o app nos dois computadores.
2. No controlador, clique em **Conectar** no computador listado em *Na rede local*. Se ele não aparecer, digite o IP mostrado no cartão *Este computador* do outro lado.
3. No computador acessado aparece o pedido. **Confira se o código de 6 dígitos é igual nos dois lados** e clique em **Aceitar**.
4. Clique na tela remota para enviar mouse e teclado. Use a barra superior para trocar o monitor, a qualidade e o FPS, e para usar tela cheia ou desconectar.
5. No computador acessado, a faixa no topo permite bloquear o controle ou encerrar o acesso a qualquer momento.

Para testar duas instâncias no mesmo computador, rode a segunda com outra pasta de dados e mude a porta dela em Configurações:

```bash
LRD_DATA_DIR=/tmp/lrd2 npm run tauri dev      # macOS
set "LRD_DATA_DIR=C:\temp\lrd2" && npm run tauri dev   # Windows (cmd)
```

---

## Arquitetura

```
apps/desktop/                 Interface (React + TypeScript + Zustand) e app Tauri 2
  src/pages, src/components   Telas: Conectar, Sessão, Configurações, Histórico, Pedido de acesso
  src/lib/renderer.ts         Desenha os quadros de tiles no <canvas>
  src/lib/keymap.ts           KeyboardEvent.code → USB HID
  src-tauri/                  Comandos IPC e ponte de eventos
crates/
  lrd-protocol   Mensagens Protobuf, enquadramento, formato binário de vídeo
  lrd-security   Identidade, fingerprints, código de verificação, permissões, política de rede
  lrd-transport  QUIC + TLS 1.3 (quinn/rustls) com mTLS
  lrd-discovery  mDNS / DNS-SD
  lrd-storage    SQLite (migrations/0001_init.sql)
  lrd-video      Captura de tela, detecção de alterações, JPEG por tile
  lrd-input      Injeção de mouse/teclado (SendInput por scancode; CGEvent no macOS)
  lrd-session    Orquestração: host (controlado) e viewer (controlador)
proto/lrd.proto  Contrato de referência
```

**Fluxo da conexão:** QUIC/TLS 1.3 com certificados dos dois lados → `Hello` (versões e codecs) → checagem do certificado contra o histórico → `ConnectRequest` → pedido na tela do computador controlado → `ConnectResponse` com as permissões concedidas e a lista de monitores → stream de vídeo (host → viewer), stream de entrada (viewer → host) e canal de controle (qualidade, monitor, permissões).

**Regras do documento atendidas:** RN-001 a RN-006, RN-008 a RN-011, RN-013 a RN-016. A RN-007 (acesso não supervisionado desativado) vale porque esse modo não existe nesta versão. A RN-012 (arquivos) fica para a fase 5.

## Onde ficam os dados

- Windows: `%APPDATA%\dev.lrd.localremotedesktop\`
- macOS: `~/Library/Application Support/dev.lrd.localremotedesktop/`

Lá ficam `identity.json` (ID e certificado público) e `lrd.sqlite3` (histórico, dispositivos conhecidos e configurações). A chave privada fica no Credential Manager ou no Keychain. Apagar a pasta gera uma nova identidade, e os outros computadores vão acusar a troca, como esperado.
