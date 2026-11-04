// Recebe os quadros da sessão e decide como apresentá-los:
// - tipo 1/2: tiles JPEG e cursor (FrameRenderer)
// - tipo 3: H.264 decodificado por hardware via WebCodecs
// - tipo 4: áudio PCM (AudioPlayer)
// Vídeo e áudio são apresentados no mesmo instante calculado por AvSync.
import { AudioPlayer } from "./audio";
import { FrameRenderer, toBytes, type CursorState } from "./renderer";
import { AvSync } from "./sync";

export interface MediaCallbacks {
  onResize: (w: number, h: number) => void;
  onCursor: (c: CursorState) => void;
  onKeyframeNeeded: () => void;
  onDrawn: () => void;
}

/** Codecs que esta interface consegue decodificar, em ordem de preferência. */
export async function supportedCodecs(): Promise<string[]> {
  const out = ["jpeg-tiles"];
  try {
    if (typeof VideoDecoder !== "undefined") {
      const r = await VideoDecoder.isConfigSupported({ codec: "avc1.42E033", optimizeForLatency: true });
      if (r.supported) out.unshift("h264");
    }
  } catch {
    /* sem WebCodecs */
  }
  return out;
}

/** Extrai a string de codec ("avc1.PPCCLL") do SPS de um quadro Annex B. */
function avcCodecString(data: Uint8Array): string | null {
  for (let i = 0; i + 7 < data.length; i++) {
    if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1 && (data[i + 3] & 0x1f) === 7) {
      const hex = (n: number) => n.toString(16).padStart(2, "0");
      return `avc1.${hex(data[i + 4])}${hex(data[i + 5])}${hex(data[i + 6])}`;
    }
  }
  return null;
}

export class MediaEngine {
  readonly sync = new AvSync();
  readonly audio: AudioPlayer | null;
  private canvas: HTMLCanvasElement | null = null;
  private ctx: CanvasRenderingContext2D | null = null;
  private tiles: FrameRenderer | null = null;
  private cb: MediaCallbacks | null = null;
  private pending: Uint8Array[] = [];
  private decoder: VideoDecoder | null = null;
  private codec = "";
  private waitKey = true;
  private queue: VideoFrame[] = [];
  private raf = 0;
  private lastKeyRequest = 0;
  private closed = false;

  constructor(audioCtx: AudioContext | null) {
    this.audio = audioCtx ? new AudioPlayer(audioCtx, this.sync) : null;
  }

  attach(canvas: HTMLCanvasElement, cb: MediaCallbacks) {
    this.canvas = canvas;
    this.ctx = canvas.getContext("2d", { alpha: false });
    this.cb = cb;
    this.tiles = new FrameRenderer(canvas, cb.onResize, cb.onCursor, () => this.requestKeyframe(), cb.onDrawn);
    const queued = this.pending;
    this.pending = [];
    for (const f of queued) this.route(f);
    cancelAnimationFrame(this.raf);
    this.raf = requestAnimationFrame(this.tick);
  }

  detach(canvas: HTMLCanvasElement) {
    if (this.canvas !== canvas) return;
    cancelAnimationFrame(this.raf);
    this.canvas = null;
    this.ctx = null;
    this.tiles = null;
    this.cb = null;
  }

  close() {
    this.closed = true;
    cancelAnimationFrame(this.raf);
    for (const f of this.queue) f.close();
    this.queue = [];
    try {
      this.decoder?.close();
    } catch {
      /* já fechado */
    }
    this.decoder = null;
    this.audio?.close();
  }

  push(data: unknown) {
    if (this.closed) return;
    const bytes = toBytes(data);
    if (!bytes || bytes.length === 0) return;
    if (bytes[0] === 4) {
      this.audio?.push(new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength));
      return;
    }
    if (!this.canvas) {
      this.pending.push(bytes);
      if (this.pending.length > 400) this.pending = []; // a tela pede um quadro completo ao abrir
      return;
    }
    this.route(bytes);
  }

  private route(bytes: Uint8Array) {
    if (bytes[0] === 3) this.pushH264(bytes);
    else this.tiles?.push(bytes);
  }

  private requestKeyframe() {
    const now = performance.now();
    if (now - this.lastKeyRequest < 500) return;
    this.lastKeyRequest = now;
    this.cb?.onKeyframeNeeded();
  }

  private makeDecoder(codec: string) {
    try {
      this.decoder?.close();
    } catch {
      /* ignora */
    }
    const dec = new VideoDecoder({
      output: (frame) => {
        this.sync.noteVideo(frame.timestamp, performance.now());
        this.queue.push(frame);
        while (this.queue.length > 8) this.queue.shift()!.close();
      },
      error: (e) => {
        console.warn("decodificador H.264:", e);
        this.decoder = null;
        this.waitKey = true;
        this.requestKeyframe();
      },
    });
    dec.configure({ codec, optimizeForLatency: true });
    this.decoder = dec;
    this.codec = codec;
  }

  private pushH264(bytes: Uint8Array) {
    if (bytes.length <= 18) return;
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const key = (view.getUint8(1) & 1) !== 0;
    const tsUs = Number(view.getBigUint64(6, true));
    const data = bytes.subarray(18);

    if (key) {
      const codec = avcCodecString(data) ?? this.codec ?? "avc1.42E033";
      const backlog = (this.decoder?.decodeQueueSize ?? 0) > 6;
      if (!this.decoder || this.decoder.state === "closed" || codec !== this.codec || backlog) {
        try {
          this.makeDecoder(codec);
        } catch (e) {
          console.warn("não foi possível configurar o H.264:", e);
          return;
        }
      }
      this.waitKey = false;
    } else {
      if (this.waitKey || !this.decoder) {
        this.requestKeyframe();
        return;
      }
      if (this.decoder.decodeQueueSize > 6) {
        // A decodificação não está acompanhando: espera o próximo quadro completo.
        this.waitKey = true;
        this.requestKeyframe();
        return;
      }
    }
    try {
      this.decoder!.decode(new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp: tsUs, data }));
    } catch (e) {
      console.warn("falha ao decodificar:", e);
      this.waitKey = true;
      this.requestKeyframe();
    }
  }

  private tick = () => {
    this.raf = requestAnimationFrame(this.tick);
    const canvas = this.canvas;
    const ctx = this.ctx;
    if (!canvas || !ctx || this.queue.length === 0) return;
    const now = performance.now();
    const synced = this.sync.audioActive(now);
    let frame: VideoFrame | null = null;
    while (this.queue.length > 0) {
      const next = this.queue[0];
      if (synced && frame && this.sync.playoutMs(next.timestamp) > now) break;
      if (synced && !frame && this.sync.playoutMs(next.timestamp) > now) return;
      frame?.close();
      frame = this.queue.shift()!;
    }
    if (!frame) return;
    const w = frame.displayWidth;
    const h = frame.displayHeight;
    if (canvas.width !== w || canvas.height !== h) {
      canvas.width = w;
      canvas.height = h;
      this.cb?.onResize(w, h);
    }
    ctx.drawImage(frame, 0, 0);
    frame.close();
    this.cb?.onDrawn();
  };
}
