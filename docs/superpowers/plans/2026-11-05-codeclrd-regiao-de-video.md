# codecLRD — Região de vídeo H.265 — Plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Mandar só a região de vídeo/jogo da tela em H.265 (H.264 de reserva) dentro do quadro codecLRD, reduzindo vídeo de ~25–55 Mbps para ≤ 8–12 Mbps, mantendo o texto ao redor 100% exato.

**Architecture:** Um detector de região (unidade pura) marca o maior grupo de blocos em movimento contínuo. O `LrdEncoder` copia a região para a referência antes de comparar, o que a esconde do caminho normal, e a codifica com um `RegionEncoder`. Em produção, o `RegionEncoder` é o `H264Encoder` em modo HEVC. O resultado vai como operação `VIDEO` no mesmo quadro. O controlador decodifica com `VideoDecoder` (WebCodecs) e desenha no retângulo.

**Tech Stack:** Rust (lrd-protocol, lrd-video, lrd-session), TypeScript/React (WebCodecs), webrtc-rs data channel, Playwright/safaridriver (testes ponta a ponta).

**Spec:** `docs/superpowers/specs/2026-11-05-codeclrd-regiao-de-video-design.md`

## Global Constraints

- Codec da região: **H.265 (HEVC)**; H.264 só quando um lado não suporta H.265. O codec vai no bit 1 das flags da operação.
- Operação nova: `5 VIDEO : x y w h, u8 flags (bit 0 = IDR, bit 1 = H.265), u32 len, [unidade de acesso Annex B]`.
- Nomes de capacidade no Hello: `lrd-video` (decodifica H.264) e `lrd-video-h265` (decodifica H.265). Testes de suporte: `avc1.42E033` e `hvc1.1.6.L120.B0`, com `optimizeForLatency: true`.
- Detector:
  - bloco de vídeo = classificado com perdas por ≥ **6** quadros seguidos;
  - liga após 6 quadros;
  - redimensiona só se algum lado mudar > **64 px**;
  - desliga após **1 s** sem movimento dentro;
  - retângulo alinhado a **16 px**, dimensões pares, mínimo **128×128**;
  - uma região por vez (o maior grupo conectado).
- Taxa da região = `bitrate_bps` do perfil × (área da região ÷ área da tela), com piso de **1 500 000 bps**.
- Falha do codificador ⇒ região desativada pelo resto da sessão (volta ao JPEG de hoje).
- Metas ponta a ponta (Windows, Chrome, 1920×1080):
  - `video-720p` ≤ **8 Mbps** e ≥ **28 qps**;
  - `video-tela-cheia` ≤ **12 Mbps** e ≥ **28 qps**;
  - caso `codecLRD` com texto **100%** exato.
- Textos visíveis ao usuário em português.

## Review Focus

1. A região desliga ou encolhe ⇒ a área liberada tem que voltar **exata** (não pode ficar o último quadro H.265 borrado para sempre). Teste na Task 3.
2. A região muda de tamanho ⇒ o primeiro quadro novo tem que ser **IDR**, e o controlador reconfigura o decodificador. Testes nas Tasks 3 e 5.
3. O controlador perde um quadro e pede quadro completo ⇒ IDR na região **e** quadro completo fora dela. Teste na Task 3.
4. Vídeo encostado no texto: os blocos de borda com texto não podem virar vídeo e borrar o texto. A região é alinhada a 16 px e só cresce sobre blocos em movimento. Teste na Task 2.
5. Navegador sem `VideoDecoder` ou sem HEVC ⇒ nada quebra: H.264 ou o caminho de hoje. Testes nas Tasks 4 e 5.

---

### Task 1: Operação VIDEO no formato

**Files:**
- Modify: `crates/lrd-protocol/src/lrd.rs` (constantes, `Writer`, `Op`, `parse`, testes)
- Modify: `crates/lrd-protocol/src/lib.rs` (módulo `codec`)

**Interfaces:**
- Produces:
  - `pub const OP_VIDEO: u8 = 5;`
  - `pub const VIDEO_IDR: u8 = 1; pub const VIDEO_HEVC: u8 = 2;`
  - `Writer::video(&mut self, x: u32, y: u32, w: u32, h: u32, flags: u8, data: &[u8])`
  - `Op::Video { x: u32, y: u32, w: u32, h: u32, flags: u8, data: &'a [u8] }`
  - `codec::LRD_VIDEO = "lrd-video"` e `codec::LRD_VIDEO_H265 = "lrd-video-h265"`

- [ ] **Step 1: Write the failing test** `video_op_roundtrip` em `lrd.rs`:
  - escreve `w.video(16, 32, 640, 368, VIDEO_IDR | VIDEO_HEVC, &[0,0,0,1,0x40])` num quadro 1280×720;
  - `parse` devolve `Op::Video { x: 16, y: 32, w: 640, h: 368, flags: 3, data: [0,0,0,1,0x40] }`;
  - um VIDEO que sai do quadro (x = 1000, w = 640) faz `parse` devolver `None`.
- [ ] **Step 2: Run** `rustc --edition 2021 --test crates/lrd-protocol/src/lrd.rs -o /tmp/lrdw && /tmp/lrdw` — Expected: erro de compilação (`video`/`Op::Video` inexistentes).
- [ ] **Step 3: Implement** as constantes, `Writer::video` (mesmo padrão de `jpeg`: retângulo + u8 flags + u32 len + dados), o ramo `OP_VIDEO` em `parse` (validando `len`) e as duas constantes em `codec`. Atualize a documentação do formato no topo do arquivo.
- [ ] **Step 4: Run** o mesmo comando — Expected: `test result: ok` (2 testes).
- [ ] **Step 5: Commit** `codecLRD: operação VIDEO no formato`.

### Task 2: Detector de região

**Files:**
- Create: `crates/lrd-video/src/lrd/region.rs` (+ `mod region;` em `lrd/mod.rs`)

**Interfaces:**
- Produces:
  - `pub struct RegionTracker` com `pub fn new(width: u32, height: u32) -> Self`
  - `pub fn update(&mut self, moving_tiles: &[bool], inside_changed: bool, now: Instant) -> Option<Rect>` — `moving_tiles`: um por bloco de 64×64 (linha a linha, `cols = width.div_ceil(64)`), verdadeiro se o bloco foi classificado com perdas neste quadro; `inside_changed`: os pixels da região ativa mudaram neste quadro. Devolve a região ativa (x, y, w, h).
  - `pub fn reset(&mut self)` — desativa (troca de resolução ou quadro completo forçado).
  - `Rect = (u32, u32, u32, u32)` (o tipo já existe em `lrd/mod.rs`; torne-o `pub(crate)`).

- [ ] **Step 1: Write the failing tests** em `region.rs` (`#[cfg(test)]`). Tela 1920×1080; blocos em movimento = retângulo de blocos (5..25, 3..15). `now` avança 33 ms por quadro.
  - `activates_after_six_frames`: nos quadros 1–5 → `None`; no quadro 6 → `Some(r)` com `r.0 % 16 == 0`, `r.1 % 16 == 0`, `r.2 % 2 == 0`, `r.3 % 2 == 0` e `r` contendo (5·64, 3·64)–(25·64, 15·64) recortado à tela.
  - `ignores_small_regions`: grupo de 1×1 bloco por 20 quadros → sempre `None` (64 < 128).
  - `picks_largest_group`: dois grupos (2×2 e 6×4 blocos) → região do grupo 6×4.
  - `stable_under_jitter`: depois de ativa, a máscara oscila ±1 bloco numa borda → mesmo retângulo em todos os quadros.
  - `resizes_on_big_change`: o grupo cresce 3 blocos para a direita (192 px) → retângulo novo mais largo.
  - `stays_while_inside_changes_then_turns_off`: ativa; em seguida `moving_tiles` todo falso e `inside_changed = true` por 60 quadros → continua ativa; depois `inside_changed = false` → ainda ativa aos 900 ms e `None` após 1 000 ms.
  - `does_not_grow_over_static_text`: grupo em movimento (5..25, 3..15) mais um bloco parado vizinho → a borda direita da região é ≤ 25·64 + 15 (só o alinhamento a 16 px pode avançar sobre o vizinho).
- [ ] **Step 2: Run** o harness local (lógica sem dependências):
  `cd $SCRATCH/lrdh && rustc --edition 2021 --test --cfg lrd_stub -O root.rs --extern lrd_protocol=liblrd_protocol.rlib -L . -o t && ./t region`
  — Expected: erro de compilação (módulo/tipo inexistente).
- [ ] **Step 3: Implement** `RegionTracker`:
  - contador de sequência por bloco (zera quando o bloco não está em movimento); um bloco é "vídeo" com contador ≥ 6;
  - maior componente 4-conectado de blocos de vídeo (BFS);
  - caixa em pixels recortada à tela; x/y arredondados para baixo a 16 px; largura e altura arredondadas para cima a 16 px, recortadas à tela e tornadas pares;
  - mínimo 128×128;
  - histerese de 64 px por lado;
  - `last_activity` atualizado quando há blocos de vídeo ou `inside_changed`; desliga quando `now - last_activity ≥ 1 s`.
- [ ] **Step 4: Run** o mesmo comando — Expected: os 7 testes passam.
- [ ] **Step 5: Commit** `codecLRD: detector de região de vídeo`.

### Task 3: Região no LrdEncoder

**Files:**
- Modify: `crates/lrd-video/src/lrd/mod.rs`
- Create: `crates/lrd-video/src/lrd/region_encoder.rs` (trait + implementação com `H264Encoder`)
- Test: `crates/lrd-video/src/lrd/tests.rs`

**Interfaces:**
- Consumes: `RegionTracker` (Task 2); `Writer::video`, `VIDEO_IDR`, `VIDEO_HEVC` (Task 1).
- Produces:
  - `pub trait RegionEncoder: Send { fn encode(&mut self, frame: &RawFrame, force_idr: bool) -> Option<RegionOut>; fn set_bitrate(&mut self, bps: u32); }`
  - `pub struct RegionOut { pub annexb: Vec<u8>, pub idr: bool, pub hevc: bool }`
  - `pub fn hardware_region_encoder(hevc: bool, fps: u32, bitrate_bps: u32) -> Box<dyn FnMut(u32, u32) -> Option<Box<dyn RegionEncoder>> + Send>` — fábrica chamada a cada tamanho de região (largura, altura). Usa `H264Encoder::new(&QualitySettings { jpeg_quality: 80, max_width: w, max_fps: fps, bitrate_bps })` + `set_hevc(hevc)`. Saída: `annexb = frame[18..]`, `idr = frame[1] & 1 != 0`, `hevc = frame[1] & 2 != 0`.
  - `LrdEncoder::enable_video(&mut self, factory: Box<dyn FnMut(u32, u32) -> Option<Box<dyn RegionEncoder>> + Send>, profile_bitrate_bps: u32)`
  - `LrdStats` ganha `video_bytes: usize` e `video_rect: Option<Rect>`.

- [ ] **Step 1: Write the failing tests** em `tests.rs`, com um `StubRegion` (implementa `RegionEncoder`: devolve `annexb` = 16 bytes contendo w, h e um contador; `idr = force_idr || primeiro quadro`; `hevc = true`; conta chamadas e IDRs num `Arc<Mutex<..>>`). Estenda o `apply_frame` de referência: `Op::Video` pinta o retângulo com uma cor fixa (marca "vídeo").
  - `video_region_replaces_jpeg_and_keeps_text_exact`: 1024×512, vídeo em (128..768, 64..448) mudando a cada quadro, texto (`ui`) em volta, 12 quadros com `enable_video` → os últimos quadros têm exatamente 1 `Op::Video`, nenhum `Op::Jpeg`; pixels fora de (128..768, 64..448) iguais ao original.
  - `region_resize_starts_with_idr`: o vídeo passa a (128..960, 64..448) → o primeiro `Op::Video` com a largura nova tem `flags & VIDEO_IDR != 0`.
  - `keyframe_request_forces_idr_and_full_frame`: `force_keyframe()` com região ativa → quadro com cabeçalho keyframe **e** `Op::Video` com IDR.
  - `released_area_returns_exact`: o vídeo para (conteúdo vira `ui`) → depois de > 1 s sem movimento (use um relógio injetável `encode_at(frame, q, now)` ou `refine_after`/tempo simulado), o decodificado da área é igual ao original.
  - `encoder_failure_disables_video`: a fábrica devolve `None` → nenhum `Op::Video`; a área vai em JPEG como hoje (o teste `unaligned_video_region_uses_few_operations` continua passando).
- [ ] **Step 2: Run** o harness local `./t video_region region_resize keyframe_request released_area encoder_failure` — Expected: erro de compilação (`enable_video`, `RegionEncoder` inexistentes).
- [ ] **Step 3: Implement** no `LrdEncoder`:
  - **Relógio injetável:** crie `pub fn encode_at(&mut self, frame, q, now: Instant)`; o `encode` atual vira `encode_at(.., Instant::now())`.
  - **Antes da comparação (passo 1):** se há região ativa, `inside_changed` = pixels da região diferentes na referência; depois copie a região de `cur` para `reference`.
  - **Depois da classificação:** `moving` = blocos `Class::Lossy`. Chame `tracker.update`.
  - **Região nova ou redimensionada:** chame a fábrica e force IDR. Se a fábrica devolver `None`, desative o vídeo pelo resto da sessão.
  - **Recorte:** copie a região (`RawFrame::cpu` BGRA) e chame `encode(&recorte, force_idr)`. Taxa: `max(1_500_000, profile_bitrate × área_região ÷ área_tela)`. Escreva `Writer::video` com `VIDEO_IDR`/`VIDEO_HEVC` conforme o `RegionOut`.
  - **Região que encolhe ou desliga:** zere a `reference` da área liberada. O próximo quadro a envia sem perdas pelo caminho normal.
  - **`force_keyframe`:** também força IDR.
  - **Quadro sem mudanças fora da região mas com vídeo:** o quadro sai com só o `Op::Video`.
- [ ] **Step 4: Run** `./t` (suíte inteira do harness) — Expected: todos passam (os 15 de antes + os 5 novos).
- [ ] **Step 5: Commit** `codecLRD: região de vídeo H.265 no codificador`.

### Task 4: Ligação host ↔ captura ↔ teste ponta a ponta

**Files:**
- Modify: `crates/lrd-video/src/pipeline.rs` (`CaptureParams.lrd_video: Option<bool>`; `Some(hevc)` ⇒ `enable_video(hardware_region_encoder(hevc, max_fps, quality.bitrate_bps), quality.bitrate_bps)`)
- Modify: `crates/lrd-session/src/host.rs` (lê `LRD_VIDEO` / `LRD_VIDEO_H265` do Hello)
- Modify: `crates/lrd-session/src/viewer.rs` (deixa passar os dois nomes, como `LRD`)
- Modify: `crates/lrd-session/examples/rtc_e2e.rs` (`/offer?lrdvideo=0|1&lrdh265=0|1` ⇒ `enable_video` no laço codecLRD; métrica `videoKbPerFrame`)

**Interfaces:**
- Consumes: `enable_video`, `hardware_region_encoder` (Task 3); constantes (Task 1).
- Produces: `CaptureParams { …, lrd_video: Option<bool> }`; no JSON do host do e2e, `lrd.videoKbPerFrame`.

- [ ] **Step 1:** Ajuste todos os pontos que constroem `CaptureParams` (`grep -rn "CaptureParams {" crates`). Host: `lrd_video = viewer_lrd_video.then_some(viewer_lrd_video_h265)`.
- [ ] **Step 2:** No `rtc_e2e`, leia `lrdvideo`/`lrdh265` da query de `/offer` e, com `--lrd`, chame `enable_video(hardware_region_encoder(h265, 30, 8_000_000), 8_000_000)`. Some `last.video_bytes` no `LrdMeter` e exponha `videoKbPerFrame`.
- [ ] **Step 3: Verify** no CI (empurre a branch): os jobs `windows` e `mac-universal` compilam e o job ponta a ponta do Windows passa. Expected: `cargo test --workspace` ok; sem `error[`.
- [ ] **Step 4: Commit** `codecLRD: região de vídeo ligada no host e no teste`.

### Task 5: Decodificação no controlador

**Files:**
- Modify: `apps/desktop/src/lib/lrdcodec.ts` (OP_VIDEO)
- Modify: `apps/desktop/src/lib/media.ts` (`supportedCodecs` anuncia `lrd-video` / `lrd-video-h265`)
- Modify: `tools/e2e/page.html` (manda `lrdvideo` / `lrdh265` em `/offer`; registra `videoCodec` usado)

**Interfaces:**
- Consumes: formato da Task 1.
- Produces: `LrdDecoder.prof.video` (ms acumulados decodificando + desenhando vídeo); `LrdDecoder.videoCodec: string | null` (último codec configurado).

- [ ] **Step 1:** Em `supportedCodecs`, chame `VideoDecoder.isConfigSupported` com `avc1.42E033` e com `hvc1.1.6.L120.B0`, ambos com `optimizeForLatency: true`. Adicione `lrd-video` e `lrd-video-h265` conforme o resultado. Proteja com try/catch: sem `VideoDecoder`, não adiciona nada.
- [ ] **Step 2:** Em `LrdDecoder`:
  - `parse` reconhece `OP_VIDEO` com a mesma validação de retângulo.
  - `apply`, no IDR, (re)configura um `VideoDecoder` quando muda o codec ou o tamanho: `codec` = `hvc1.1.6.L120.B0` se `flags & 2`, senão `avc1.42E033`; `codedWidth/codedHeight` = w/h; `optimizeForLatency: true`.
  - Decodifica com `EncodedVideoChunk({ type: idr ? "key" : "delta", timestamp: frameId, data })`.
  - Aguarda o `VideoFrame` de saída (promessa resolvida no callback `output`, com tempo-limite de 500 ms), `drawImage(frame, x, y, w, h)`, `frame.close()`. Só então aplica as demais operações.
  - Delta sem decodificador configurado, erro ou tempo-limite ⇒ `needKeyframe()`, como os outros erros.
- [ ] **Step 3: Verify** tipos: `cd /tmp/claude-0/tscheck && tsc -p .` — Expected: nenhum erro novo em `lib/`.
- [ ] **Step 4: Commit** `codecLRD: controlador decodifica a região de vídeo (WebCodecs)`.

### Task 6: Critérios ponta a ponta, diagnóstico e publicação

**Files:**
- Modify: `tools/e2e/run.mjs` (cenários com metas; coluna "vídeo KB/q" e codec)
- Modify: `.github/workflows/build.yml` (o passo de cenários deixa de ser só informativo para `video-720p` e `video-tela-cheia` no Windows)
- Modify: `apps/desktop/src/pages/RemoteSessionPage.tsx` (barra: "codecLRD · região de vídeo H.265 · X Mbps")
- Modify: `apps/desktop/src-tauri/tauri.conf.json`, `apps/desktop/package.json`, `Cargo.toml` (versão 0.5.0)

- [ ] **Step 1:** Em `run.mjs`, para cenários: `video-720p` falha se Mbps > 8 ou qps < 28; `video-tela-cheia` falha se Mbps > 12 ou qps < 28 (só em Windows — `process.platform === "win32"`); no Mac, só registra. `digitando`, `rolagem` e `arrastar-janela` não podem piorar mais de 20% em KB/quadro em relação a hoje (rolagem 15, arrastar 125, digitando 1,6 KB/q).
- [ ] **Step 2:** No 📊 e na barra, mostre `video_rect`/codec. O host envia `HostStats.encoder` com "codecLRD + vídeo H.265 W×H" quando a região está ativa (reaproveite `Encoder::name`).
- [ ] **Step 3: Verify** no CI. Expected:
  - Windows: `video-720p` ≤ 8 Mbps e ≥ 28 qps; `video-tela-cheia` ≤ 12 Mbps e ≥ 28 qps; caso `codecLRD` 100% exato.
  - Mac: Safari decodifica `video/H265` (`codec` = `hvc1…` nos registros) sem travadas.
- [ ] **Step 4:** Versão 0.5.0; commit com `[publicar]`; links dos instaladores.
- [ ] **Step 5: Commit** `Versão 0.5.0: região de vídeo H.265 [publicar]`.
