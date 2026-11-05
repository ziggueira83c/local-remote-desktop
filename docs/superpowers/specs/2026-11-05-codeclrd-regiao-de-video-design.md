# codecLRD — região de vídeo com codec temporal (caminho A)

**Data:** 05/11/2026
**Base:** especificação DeskRT/DeskStream, seção 3.5 ("codec temporal opcional" para movimento contínuo).

## 1. Objetivo

Hoje, vídeo e jogo vão como JPEG quadro a quadro: ~227 KB por quadro 1080p, ou seja, 50+ Mbps a 30 qps. Esta mudança manda **só a região onde há vídeo/jogo** para o codificador de vídeo da placa (**H.265**, com H.264 só de reserva), que envia apenas a diferença entre quadros. Texto e janelas continuam no codecLRD, sem perdas.

**Critérios de sucesso** (medidos no teste ponta a ponta do GitHub, Windows + Chrome, cenários 1920×1080):

| Cenário | Hoje | Meta |
|---|---|---|
| vídeo 720p dentro da tela | ~25 Mbps, 30 qps | **≤ 8 Mbps**, ≥ 28 qps |
| vídeo em tela cheia | ~55 Mbps, 30 qps | **≤ 12 Mbps**, ≥ 28 qps |
| texto parado ao redor do vídeo | 100% exato | **continua 100% exato** |
| digitando / rolagem / arrastar janela | como hoje | **sem piora** |

No Safari (motor do app no Mac): o vídeo da região tem que ser decodificado e aparecer, ainda que a máquina virtual do GitHub limite os quadros por segundo.

**O que o usuário pediu:** reduzir a banda de vídeo e jogo bem abaixo de 50 Mbps e subir os quadros por segundo, sem perder o texto 100% nítido. Ele aceitou usar H.264/H.265 só dentro da região que se mexe.

**Decisões (aprovadas em 05/11, 15:58 — "pode decidir o melhor para desempenho sem perder qualidade; usar H.265"):**
- **H.265 (HEVC)** é o codec da região. H.264 entra só como reserva automática quando um dos lados não suporta H.265 (codificador de hardware ausente no host, ou `VideoDecoder` sem HEVC no controlador).
- Uma região de vídeo por vez (o maior vídeo da tela); um segundo vídeo simultâneo continua em JPEG.

## 2. Arquitetura

```text
captura → LrdEncoder ─┬─ detector de região de vídeo (blocos em movimento contínuo)
                      ├─ fora da região: codecLRD de hoje (FILL / LZ4 / JPEG / COPY)
                      └─ dentro da região: recorte → H264Encoder em modo HEVC (NVENC / VideoToolbox; reserva H.264/OpenH264)
                         └─ operação nova VIDEO dentro do mesmo quadro codecLRD
controlador → LrdDecoder ─┬─ operações de hoje (canvas)
                          └─ VIDEO → VideoDecoder (WebCodecs) → desenha o VideoFrame no retângulo
```

Tudo continua num quadro codecLRD só, pelo mesmo canal de dados confiável e ordenado, com a mesma confirmação de quadro aplicado (controle de fluxo). Não há segundo canal nem sincronia extra.

### 2.1 Detector de região (`lrd/region.rs`, novo)

- Por bloco de 64×64: um bloco é "vídeo" quando foi classificado com perdas em movimento (`Lossy`) por **≥ 6 quadros seguidos** (~200 ms).
- A região é o retângulo que envolve o **maior grupo conectado** de blocos de vídeo, alinhado a 16 px (macrobloco do H.264), com largura e altura pares e no mínimo 128×128.
- **Histerese**, para não recriar o codificador o tempo todo (cada recriação custa um quadro completo):
  - liga depois de 6 quadros de movimento;
  - só muda de tamanho quando a nova região difere em mais de 64 px de algum lado;
  - desliga depois de **1 s sem movimento** dentro dela.
- É uma unidade pura: entrada = máscara de blocos em movimento por quadro; saída = `Option<Rect>`. Testável sem codificador.

### 2.2 Exclusão da região no codecLRD

- A cada quadro com região ativa, os pixels da região são copiados de `cur` para `reference` **antes** da comparação. Assim os retângulos de mudança e a classificação simplesmente não enxergam a região, e nenhuma lógica de hoje muda.
- Quando a região desliga ou encolhe, a área que saiu dela é marcada como "suja" (a referência daquela área é invalidada). No quadro seguinte ela vai sem perdas pelo caminho normal e o texto volta exato.

### 2.3 Codificação da região

- Recorte BGRA da região → `H264Encoder` (fachada atual) com `set_hevc(true)` quando o controlador decodifica H.265: NVENC HEVC (RTX 2060S) / VideoToolbox HEVC (Mac 2019). Se o hardware não tiver H.265, a fachada já cai para H.264 (e o OpenH264 é a última reserva). O codec usado vai marcado em cada operação.
- Uma instância por região; recriada quando o tamanho muda. O primeiro quadro após criar é IDR.
- **Taxa:** `bitrate_bps` do perfil de qualidade (Fluidez/Equilibrado/Nitidez) × fração da tela que a região ocupa, com piso de 1,5 Mbps. Exemplo: Equilibrado 8 Mbps, vídeo 720p em tela 1080p ⇒ ~3,6 Mbps.
- Pedido de quadro completo do controlador (`Keyframe`) ⇒ IDR na região + quadro completo do codecLRD fora dela.
- Falha do codificador ⇒ região desativada pela sessão inteira; a área volta para o JPEG de hoje.

### 2.4 Formato (`lrd-protocol/src/lrd.rs`)

Operação nova, retrocompatível com o cabeçalho atual:

```text
5 VIDEO : x y w h, u8 flags (bit 0 = IDR, bit 1 = H.265), u32 len, [unidade de acesso Annex B]
```

O controlador declara no Hello:
- `lrd-video` se decodifica H.264: `VideoDecoder.isConfigSupported({codec: "avc1.42E033", optimizeForLatency: true})`;
- `lrd-video-h265` se decodifica H.265: o mesmo teste com `codec: "hvc1.1.6.L120.B0"`.

O host só usa VIDEO com `lrd-video` e só pede H.265 ao codificador com `lrd-video-h265`. Sem nenhum dos dois, tudo segue como hoje.

### 2.5 Decodificação (`lrdcodec.ts`)

- Um `VideoDecoder` por região. É configurado no IDR com o codec indicado no bit 1 (`hvc1.1.6.L120.B0` para H.265 Main; `avc1` com perfil/nível do SPS para H.264), `codedWidth/Height = w×h` e `optimizeForLatency: true`. Mudança de codec ou de tamanho ⇒ reconfigura no próximo IDR.
- Ao aplicar o quadro: entrega a unidade de acesso e **espera o `VideoFrame` correspondente**. Não há quadros B, então sai 1 para 1. Depois desenha com `drawImage(frame, x, y, w, h)`, fecha o frame e só então aplica as demais operações e confirma o quadro.
- Erro do decodificador ou quadro não-IDR sem decodificador configurado ⇒ pede quadro completo (caminho já existente).

## 3. Diagnóstico

- `LrdStats` ganha `video_bytes` e `video_rect`; a barra da sessão e o 📊 mostram "região de vídeo: W×H, X Mbps".
- O teste ponta a ponta passa a imprimir os Mbps da região e o total.

## 4. Testes

- **Unidade (Rust):**
  - detector liga após 6 quadros, não oscila com mudanças pequenas e desliga após 1 s;
  - alinhamento a 16 px;
  - exclusão: texto ao redor da região continua exato;
  - ida e volta do formato VIDEO;
  - região que sai volta exata pelo caminho sem perdas.
- **Ponta a ponta:** os cenários `video-720p` e `video-tela-cheia` têm as metas de Mbps e qps da seção 1 como critério (OpenH264 no GitHub; NVENC/VideoToolbox só nos computadores reais). O caso `codecLRD` atual (texto 100% exato) continua obrigatório.

## 5. Fora do escopo

- Áreas alteradas e janelas movidas informadas pelo Windows/macOS.
- Cópia 2D (arrastar janela).
- Renderizador WebGL.
- Mais de uma região de vídeo.
- Captura direta da GPU para a região: o recorte é feito na CPU, porque o quadro já está na memória para o codecLRD.

Cada um vira uma mudança separada depois.
