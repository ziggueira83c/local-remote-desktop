// Decodificador do codecLRD (formato em crates/lrd-protocol/src/lrd.rs).
//
// Mantém o estado da tela no <canvas> e aplica cada quadro em ordem. Quadro fora de ordem
// (frame_id pulado) ou erro ⇒ para de aplicar e pede um quadro completo.

export const FRAME_LRD = 5;
const HEADER_LEN = 20;
const OP_FILL = 1;
const OP_LZ4 = 2;
const OP_JPEG = 3;
const OP_COPY = 4;
const OP_VIDEO = 5;
const VIDEO_IDR = 1;
const VIDEO_HEVC = 2;
/** Codecs da região de vídeo (WebCodecs). */
export const REGION_CODEC_H264 = "avc1.42E033";
export const REGION_CODEC_H265 = "hvc1.1.6.L120.B0";
const FILTER_LEFT = 1;

/** Descompressão de um bloco LZ4 (formato "block", sem cabeçalho de frame). */
export function lz4Decompress(src: Uint8Array, rawLen: number): Uint8Array {
  const dst = new Uint8Array(rawLen);
  let s = 0;
  let d = 0;
  const end = src.length;
  while (s < end) {
    const token = src[s++];
    let lit = token >> 4;
    if (lit === 15) {
      let b;
      do {
        b = src[s++];
        lit += b;
      } while (b === 255 && s < end);
    }
    if (d + lit > rawLen || s + lit > end) throw new Error("LZ4: literais fora do limite");
    dst.set(src.subarray(s, s + lit), d);
    s += lit;
    d += lit;
    if (s >= end) break; // última sequência: só literais
    const offset = src[s] | (src[s + 1] << 8);
    s += 2;
    if (offset === 0 || offset > d) throw new Error("LZ4: deslocamento inválido");
    let len = (token & 15) + 4;
    if ((token & 15) === 15) {
      let b;
      do {
        b = src[s++];
        len += b;
      } while (b === 255 && s < end);
    }
    if (d + len > rawLen) throw new Error("LZ4: cópia fora do limite");
    let m = d - offset;
    if (offset >= len) {
      dst.copyWithin(d, m, m + len);
      d += len;
    } else {
      // Sobreposição (padrão repetido): byte a byte.
      for (let k = 0; k < len; k++) dst[d++] = dst[m++];
    }
  }
  if (d !== rawLen) throw new Error("LZ4: tamanho final diferente");
  return dst;
}

interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

type Op =
  | (Rect & { kind: typeof OP_FILL; b: number; g: number; r: number })
  | (Rect & { kind: typeof OP_LZ4; filter: number; rawLen: number; data: Uint8Array })
  | (Rect & { kind: typeof OP_JPEG; img: Promise<ImageBitmap> })
  | (Rect & { kind: typeof OP_COPY; sx: number; sy: number })
  | (Rect & { kind: typeof OP_VIDEO; flags: number; data: Uint8Array });

export interface LrdFrameInfo {
  keyframe: boolean;
  width: number;
  height: number;
  captureUs: number;
  frameId: number;
}

export class LrdDecoder {
  /** Diagnóstico: ms acumulados por tipo de operação e quantidade de operações. */
  prof = { fill: 0, lz4: 0, jpeg: 0, copy: 0, jpegWait: 0, video: 0, ops: 0, frames: 0 };
  /** Codec da região de vídeo em uso ("hvc1…"/"avc1…"), para diagnóstico. */
  videoCodec: string | null = null;
  // Região de vídeo: decodificador (WebCodecs) e onde desenhar cada quadro decodificado.
  private vdec: VideoDecoder | null = null;
  private vconfig = "";
  private vrects = new Map<number, Rect>();
  private vwaiter: (() => void) | null = null;
  private expected = -1;
  private waitingKey = true;
  private ctx: CanvasRenderingContext2D | null;
  width = 0;
  height = 0;

  constructor(
    private canvas: HTMLCanvasElement,
    private onResize: (w: number, h: number) => void,
    private needKeyframe: () => void,
  ) {
    this.ctx = canvas.getContext("2d", { alpha: false });
  }

  /** Lê o cabeçalho; `null` se não for um quadro codecLRD válido. */
  static header(b: Uint8Array): LrdFrameInfo | null {
    if (b.length < HEADER_LEN || b[0] !== FRAME_LRD) return null;
    const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
    return {
      keyframe: (b[1] & 1) !== 0,
      width: v.getUint16(2, true),
      height: v.getUint16(4, true),
      captureUs: Number(v.getBigUint64(6, true)),
      frameId: v.getUint32(14, true),
    };
  }

  /** Aplica um quadro. Devolve as informações do quadro se ele foi desenhado. */
  async apply(b: Uint8Array): Promise<LrdFrameInfo | null> {
    const info = LrdDecoder.header(b);
    if (!info) return null;
    if (info.keyframe) {
      this.waitingKey = false;
    } else if (this.waitingKey || info.frameId !== this.expected) {
      // Faltou um quadro: o estado daqui não bate mais com o do host.
      this.waitingKey = true;
      this.needKeyframe();
      return null;
    }
    this.expected = (info.frameId + 1) >>> 0;

    let ops: Op[];
    try {
      ops = this.parse(b, info);
    } catch (e) {
      console.warn("codecLRD: quadro inválido", e);
      this.waitingKey = true;
      this.needKeyframe();
      return null;
    }
    if (info.width !== this.width || info.height !== this.height) {
      this.width = info.width;
      this.height = info.height;
      this.canvas.width = info.width;
      this.canvas.height = info.height;
      this.ctx = this.canvas.getContext("2d", { alpha: false });
      this.onResize(info.width, info.height);
    }
    // JPEGs decodificam em paralelo; as operações são aplicadas na ordem do quadro.
    const tWait = performance.now();
    const settled = await Promise.allSettled(ops.map((o) => (o.kind === OP_JPEG ? o.img : null)));
    this.prof.jpegWait += performance.now() - tWait;
    const bitmaps = settled.map((r) => (r.status === "fulfilled" ? r.value : null));
    if (settled.some((r) => r.status === "rejected")) {
      // Um bloco não decodificou: a tela daqui ficaria errada ⇒ pede um quadro completo.
      console.warn("codecLRD: falha ao decodificar JPEG", settled.find((r) => r.status === "rejected"));
      for (const bm of bitmaps) bm?.close();
      this.waitingKey = true;
      this.needKeyframe();
      return null;
    }
    this.prof.ops += ops.length;
    this.prof.frames++;
    const ctx = this.ctx;
    if (!ctx) return null;
    try {
      // Quadro sem vídeo: termina de desenhar o vídeo pendente antes (senão um quadro de
      // vídeo atrasado poderia cobrir a área que acabou de voltar sem perdas).
      if (this.vdec && !ops.some((o) => o.kind === OP_VIDEO) && this.vdec.decodeQueueSize + this.vrects.size > 0) {
        await this.vdec.flush().catch(() => {});
      }
      for (let i = 0; i < ops.length; i++) {
        const o = ops[i];
        if (o.kind === OP_VIDEO) await this.video(o, info.frameId);
        else this.draw(ctx, o, bitmaps[i]);
      }
    } catch (e) {
      console.warn("codecLRD: falha ao desenhar", e);
      this.waitingKey = true;
      this.needKeyframe();
      return null;
    } finally {
      for (const bm of bitmaps) bm?.close();
    }
    return info;
  }

  /**
   * Entrega a unidade de acesso ao decodificador de vídeo. O quadro decodificado é desenhado
   * no retângulo dele assim que sai (callback `output`); aqui espera no máximo um pouco por
   * ele, para o quadro ficar pronto junto com o resto quando o decodificador é rápido.
   */
  private async video(o: Extract<Op, { kind: typeof OP_VIDEO }>, frameId: number) {
    const t0 = performance.now();
    const idr = (o.flags & VIDEO_IDR) !== 0;
    const codec = o.flags & VIDEO_HEVC ? REGION_CODEC_H265 : REGION_CODEC_H264;
    const config = `${codec}|${o.w}|${o.h}`;
    if (idr && (config !== this.vconfig || !this.vdec || this.vdec.state !== "configured")) {
      this.closeVideo();
      const dec = new VideoDecoder({
        output: (frame) => {
          const r = this.vrects.get(frame.timestamp);
          this.vrects.delete(frame.timestamp);
          if (r && this.ctx && dec === this.vdec) this.ctx.drawImage(frame, r.x, r.y, r.w, r.h);
          frame.close();
          const wake = this.vwaiter;
          this.vwaiter = null;
          wake?.();
        },
        error: (e) => {
          console.warn("codecLRD: erro no decodificador de vídeo", e);
          if (dec === this.vdec) {
            this.closeVideo();
            this.waitingKey = true;
            this.needKeyframe();
          }
        },
      });
      dec.configure({ codec, codedWidth: o.w, codedHeight: o.h, optimizeForLatency: true });
      this.vdec = dec;
      this.vconfig = config;
      this.videoCodec = codec;
    }
    if (!this.vdec || this.vconfig !== config) {
      throw new Error("vídeo sem quadro inicial (IDR)");
    }
    // Timestamp único por quadro (o frame_id do codecLRD), usado para achar o retângulo.
    this.vrects.set(frameId, { x: o.x, y: o.y, w: o.w, h: o.h });
    const done = new Promise<void>((resolve) => {
      this.vwaiter = resolve;
      setTimeout(resolve, 40);
    });
    this.vdec.decode(new EncodedVideoChunk({ type: idr ? "key" : "delta", timestamp: frameId, data: o.data }));
    await done;
    this.prof.video += performance.now() - t0;
  }

  private closeVideo() {
    try {
      this.vdec?.close();
    } catch {
      /* já fechado */
    }
    this.vdec = null;
    this.vconfig = "";
    this.vrects.clear();
  }

  reset() {
    this.closeVideo();
    this.waitingKey = true;
    this.expected = -1;
  }

  private parse(b: Uint8Array, info: LrdFrameInfo): Op[] {
    const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
    const count = v.getUint16(18, true);
    const ops: Op[] = [];
    let o = HEADER_LEN;
    const inside = (x: number, y: number, w: number, h: number) =>
      w > 0 && h > 0 && x + w <= info.width && y + h <= info.height;
    for (let i = 0; i < count; i++) {
      if (o + 9 > b.length) throw new Error("operação truncada");
      const kind = b[o];
      const x = v.getUint16(o + 1, true);
      const y = v.getUint16(o + 3, true);
      const w = v.getUint16(o + 5, true);
      const h = v.getUint16(o + 7, true);
      if (!inside(x, y, w, h)) throw new Error("retângulo fora do quadro");
      o += 9;
      switch (kind) {
        case OP_FILL:
          ops.push({ kind, x, y, w, h, b: b[o], g: b[o + 1], r: b[o + 2] });
          o += 3;
          break;
        case OP_LZ4: {
          const filter = b[o];
          const rawLen = v.getUint32(o + 1, true);
          const len = v.getUint32(o + 5, true);
          o += 9;
          if (rawLen !== w * h * 3 || o + len > b.length) throw new Error("LZ4 com tamanho inválido");
          ops.push({ kind, x, y, w, h, filter, rawLen, data: b.subarray(o, o + len) });
          o += len;
          break;
        }
        case OP_JPEG: {
          const len = v.getUint32(o, true);
          o += 4;
          if (o + len > b.length) throw new Error("JPEG truncado");
          const blob = new Blob([b.slice(o, o + len)], { type: "image/jpeg" });
          ops.push({ kind, x, y, w, h, img: createImageBitmap(blob) });
          o += len;
          break;
        }
        case OP_COPY: {
          const sx = v.getUint16(o, true);
          const sy = v.getUint16(o + 2, true);
          o += 4;
          if (!inside(sx, sy, w, h)) throw new Error("cópia fora do quadro");
          ops.push({ kind, x, y, w, h, sx, sy });
          break;
        }
        case OP_VIDEO: {
          const flags = b[o];
          const len = v.getUint32(o + 1, true);
          o += 5;
          if (o + len > b.length) throw new Error("vídeo truncado");
          ops.push({ kind, x, y, w, h, flags, data: b.subarray(o, o + len) });
          o += len;
          break;
        }
        default:
          throw new Error(`operação desconhecida ${kind}`);
      }
    }
    if (o !== b.length) throw new Error("bytes sobrando no quadro");
    return ops;
  }

  private draw(ctx: CanvasRenderingContext2D, o: Op, bitmap: ImageBitmap | null) {
    const t0 = performance.now();
    this.drawOp(ctx, o, bitmap);
    const dt = performance.now() - t0;
    const p = this.prof;
    if (o.kind === OP_FILL) p.fill += dt;
    else if (o.kind === OP_LZ4) p.lz4 += dt;
    else if (o.kind === OP_JPEG) p.jpeg += dt;
    else p.copy += dt;
  }

  private drawOp(ctx: CanvasRenderingContext2D, o: Op, bitmap: ImageBitmap | null) {
    switch (o.kind) {
      case OP_FILL:
        ctx.fillStyle = `rgb(${o.r},${o.g},${o.b})`;
        ctx.fillRect(o.x, o.y, o.w, o.h);
        break;
      case OP_LZ4: {
        const raw = lz4Decompress(o.data, o.rawLen);
        const img = new ImageData(o.w, o.h);
        const px = img.data;
        let k = 0;
        let p = 0;
        for (let r = 0; r < o.h; r++) {
          let pb = 0;
          let pg = 0;
          let pr = 0;
          for (let c = 0; c < o.w; c++) {
            let bb = raw[k];
            let gg = raw[k + 1];
            let rr = raw[k + 2];
            if (o.filter === FILTER_LEFT) {
              bb = (bb + pb) & 255;
              gg = (gg + pg) & 255;
              rr = (rr + pr) & 255;
              pb = bb;
              pg = gg;
              pr = rr;
            }
            px[p] = rr;
            px[p + 1] = gg;
            px[p + 2] = bb;
            px[p + 3] = 255;
            k += 3;
            p += 4;
          }
        }
        ctx.putImageData(img, o.x, o.y);
        break;
      }
      case OP_JPEG:
        if (bitmap) ctx.drawImage(bitmap, o.x, o.y, o.w, o.h);
        break;
      case OP_COPY: {
        // getImageData copia antes de escrever: equivale a memmove.
        const img = ctx.getImageData(o.sx, o.sy, o.w, o.h);
        ctx.putImageData(img, o.x, o.y);
        break;
      }
    }
  }
}
