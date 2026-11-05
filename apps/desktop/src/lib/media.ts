// Recebe os quadros da sessão e decide como apresentá-los:
// - tipo 1/2: tiles JPEG e cursor (FrameRenderer)
// - tipo 3: H.264 decodificado por hardware via WebCodecs
// - tipo 4: áudio PCM (AudioPlayer)
// Vídeo e áudio são apresentados no mesmo instante calculado por AvSync.
import { FRAME_LRD, LrdDecoder } from "./lrdcodec";
import { AudioPlayer } from "./audio";
import { FrameRenderer, toBytes, type CursorState } from "./renderer";
import { AvSync } from "./sync";

export type MediaMode = "pending" | "webrtc" | "quic" | "lrd";

export interface RtcStats {
  fps: number;
  kbps: number;
  rttMs: number | null;
  jitterMs: number | null;
  lossPct: number;
}

/** Atraso medido de ponta a ponta. */
export interface LatencyStats {
  /** Captura no host → quadro na tela daqui (mediana dos últimos quadros). */
  glassMs: number | null;
  /** Ida e volta de um comando (canal de entrada). */
  inputRttMs: number | null;
  /** Comandos indo pelo canal direto do WebRTC (e não pelo caminho de reserva). */
  directInput: boolean;
}

export type InputMsg =
  | { type: "pointer"; displayId?: string; x: number; y: number }
  | { type: "button"; button: number; pressed: boolean }
  | { type: "scroll"; dx: number; dy: number }
  | { type: "key"; hid: number; pressed: boolean };

export interface MediaCallbacks {
  onMode: (m: MediaMode) => void;
  /** O áudio do WebRTC precisa de um clique para tocar (política de autoplay). */
  onAudioBlocked: (blocked: boolean) => void;
  onResize: (w: number, h: number) => void;
  onCursor: (c: CursorState) => void;
  onKeyframeNeeded: () => void;
  onDrawn: () => void;
}

/** Codecs que esta interface consegue decodificar, em ordem de preferência. */
export async function supportedCodecs(): Promise<string[]> {
  // codecLRD primeiro: é o codec próprio do app (decodificado aqui mesmo, sem depender do navegador).
  const out = ["lrd", "jpeg-tiles"];
  if (typeof RTCPeerConnection !== "undefined") {
    out.unshift("webrtc");
    try {
      // H.265 no WebRTC só quando o navegador sabe decodificar (normalmente por hardware).
      const caps = RTCRtpReceiver.getCapabilities?.("video");
      if (caps?.codecs.some((c) => c.mimeType.toLowerCase() === "video/h265")) out.push("webrtc-h265");
    } catch {
      /* sem getCapabilities */
    }
  }
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
  private pc: RTCPeerConnection | null = null;
  private stream: MediaStream | null = null;
  private video: HTMLVideoElement | null = null;
  mode: MediaMode = "pending";
  // Entrada direta (canais de dados do WebRTC) e medição de atraso.
  private dcPointer: RTCDataChannel | null = null;
  private dcInput: RTCDataChannel | null = null;
  private moveSeq = 0;
  private buttonsDown = 0;
  private heldPointer: Uint8Array | null = null;
  private pingTimer = 0;
  private clock: { offsetMs: number; rttMs: number; at: number }[] = [];
  private inputRtt: number[] = [];
  private glass: number[] = [];
  private lrd: LrdDecoder | null = null;
  private lrdChain: Promise<void> = Promise.resolve();
  private lrdBacklog = 0;
  private chunks: Uint8Array[] = [];
  private lrdFrames = 0;
  /** Diagnóstico: ms desde a chegada até desenhar (inclui fila) e só aplicando. */
  lrdTimes = { queueAndApply: [] as number[], apply: [] as number[], resets: 0 };
  private lrdBytes = 0;
  private lrdMark = { time: performance.now(), frames: 0, bytes: 0 };
  private lastStats: { bytes: number; time: number; lost: number; recv: number } | null = null;

  constructor(audioCtx: AudioContext | null) {
    this.audio = audioCtx ? new AudioPlayer(audioCtx, this.sync) : null;
  }

  private setMode(m: MediaMode) {
    if (this.mode === m) return;
    this.mode = m;
    this.cb?.onMode(m);
  }

  /** Responde a oferta WebRTC do host (só candidatos locais; sem servidores externos). */
  async acceptOffer(sdp: string): Promise<string> {
    this.pc?.close();
    const pc = new RTCPeerConnection({ iceServers: [] });
    this.pc = pc;
    pc.ontrack = (ev) => {
      // Latência mínima: o navegador mantém o menor buffer possível.
      const r = ev.receiver as RTCRtpReceiver & { jitterBufferTarget?: number; playoutDelayHint?: number };
      try {
        r.jitterBufferTarget = 0;
      } catch {
        /* não suportado */
      }
      try {
        r.playoutDelayHint = 0;
      } catch {
        /* não suportado */
      }
      if (!this.stream) this.stream = ev.streams[0] ?? new MediaStream();
      if (!this.stream.getTracks().includes(ev.track)) this.stream.addTrack(ev.track);
      this.bindVideo();
      if (ev.track.kind === "video") this.setMode("webrtc");
    };
    pc.onconnectionstatechange = () => {
      if (pc.connectionState === "failed" && this.mode === "webrtc") this.setMode("pending");
    };
    pc.ondatachannel = (ev) => this.bindChannel(ev.channel);
    await pc.setRemoteDescription({ type: "offer", sdp });
    const answer = await pc.createAnswer();
    await pc.setLocalDescription(answer);
    await new Promise<void>((resolve) => {
      if (pc.iceGatheringState === "complete") return resolve();
      const done = () => {
        pc.removeEventListener("icegatheringstatechange", check);
        resolve();
      };
      const check = () => pc.iceGatheringState === "complete" && done();
      pc.addEventListener("icegatheringstatechange", check);
      setTimeout(done, 1500);
    });
    return pc.localDescription?.sdp ?? answer.sdp ?? "";
  }

  private bindChannel(dc: RTCDataChannel) {
    dc.binaryType = "arraybuffer";
    if (dc.label === "pointer") {
      this.dcPointer = dc;
      dc.bufferedAmountLowThreshold = 512;
      dc.onbufferedamountlow = () => {
        const held = this.heldPointer;
        this.heldPointer = null;
        if (held && dc.readyState === "open") dc.send(held as unknown as ArrayBuffer);
      };
    } else if (dc.label === "video") {
      // Quadros do codecLRD (e cursor) em pedaços: 1º byte bit 7 = início, bit 6 = fim.
      dc.onmessage = (ev) => {
        const b = new Uint8Array(ev.data as ArrayBuffer);
        if (b.length === 0) return;
        if (b[0] & 0x80) this.chunks = [];
        this.chunks.push(b.subarray(1));
        if (b[0] & 0x40) {
          const parts = this.chunks;
          this.chunks = [];
          let n = 0;
          for (const p of parts) n += p.length;
          const frame = new Uint8Array(n);
          let o = 0;
          for (const p of parts) {
            frame.set(p, o);
            o += p.length;
          }
          this.push(frame);
        }
      };
    } else if (dc.label === "input") {
      this.dcInput = dc;
      dc.onmessage = (ev) => this.onControlMessage(new Uint8Array(ev.data as ArrayBuffer));
      const start = () => {
        // Sincroniza os relógios: rajada inicial e depois a cada segundo.
        for (let i = 0; i < 5; i++) setTimeout(() => this.ping(), i * 60);
        clearInterval(this.pingTimer);
        this.pingTimer = window.setInterval(() => this.ping(), 1000);
      };
      if (dc.readyState === "open") start();
      else dc.onopen = start;
    }
  }

  private ping() {
    const dc = this.dcInput;
    if (!dc || dc.readyState !== "open") return;
    const b = new Uint8Array(9);
    const v = new DataView(b.buffer);
    v.setUint8(0, 5);
    v.setFloat64(1, performance.timeOrigin + performance.now(), true);
    dc.send(b);
  }

  private onControlMessage(b: Uint8Array) {
    if (b.length < 17 || b[0] !== 6) return;
    const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
    const t0 = v.getFloat64(1, true);
    const hostMs = Number(v.getBigUint64(9, true)) / 1000;
    const now = performance.timeOrigin + performance.now();
    const rtt = now - t0;
    if (rtt < 0 || rtt > 2000) return;
    // O instante do host corresponde, em média, ao meio da ida e volta.
    this.clock.push({ offsetMs: hostMs - (t0 + rtt / 2), rttMs: rtt, at: now });
    this.clock = this.clock.filter((c) => now - c.at < 30_000).slice(-30);
    this.inputRtt.push(rtt);
    if (this.inputRtt.length > 10) this.inputRtt.shift();
  }

  /** Diferença relógio do host − relógio local (ms), da amostra com menor ida e volta. */
  private clockOffset(): number | null {
    if (this.clock.length === 0) return null;
    let best = this.clock[0];
    for (const c of this.clock) if (c.rttMs < best.rttMs) best = c;
    return best.offsetMs;
  }

  /**
   * Chamado para cada quadro apresentado (requestVideoFrameCallback). O carimbo RTP do host
   * é o instante da captura (90 kHz), então dá para medir captura → tela.
   */
  noteVideoFrame(meta: { rtpTimestamp?: number; expectedDisplayTime?: number; presentationTime?: number }) {
    const off = this.clockOffset();
    if (off == null || meta.rtpTimestamp == null) return;
    const shown = performance.timeOrigin + (meta.expectedDisplayTime ?? meta.presentationTime ?? performance.now());
    const hostTicks = Math.round((shown + off) * 90) % 4294967296;
    let diff = hostTicks - (meta.rtpTimestamp >>> 0);
    if (diff < 0) diff += 4294967296;
    const ms = diff / 90;
    if (ms > 5000) return; // fora da faixa (relógio ainda não sincronizado)
    this.glass.push(ms);
    if (this.glass.length > 60) this.glass.shift();
  }

  /** Diagnóstico do decodificador codecLRD (ms acumulados por etapa). */
  lrdProfile() {
    return this.lrd?.prof ?? null;
  }

  /** Quadros/s e banda do codecLRD desde a última chamada. */
  lrdStats(): { fps: number; kbps: number } | null {
    if (this.mode !== "lrd") return null;
    const now = performance.now();
    const dt = (now - this.lrdMark.time) / 1000;
    if (dt <= 0) return null;
    const r = {
      fps: (this.lrdFrames - this.lrdMark.frames) / dt,
      kbps: ((this.lrdBytes - this.lrdMark.bytes) * 8) / 1000 / dt,
    };
    this.lrdMark = { time: now, frames: this.lrdFrames, bytes: this.lrdBytes };
    return r;
  }

  latency(): LatencyStats {
    const median = (a: number[]) => {
      if (a.length === 0) return null;
      const s = [...a].sort((x, y) => x - y);
      return s[Math.floor(s.length / 2)];
    };
    return {
      glassMs: this.mode === "webrtc" || this.mode === "lrd" ? median(this.glass) : null,
      inputRttMs: median(this.inputRtt),
      directInput: this.dcInput?.readyState === "open",
    };
  }

  /**
   * Envia um comando direto ao host pelo WebRTC (sem passar pelo processo do app).
   * Retorna `false` se o canal não estiver aberto (o chamador usa o caminho de reserva).
   */
  sendInput(m: InputMsg): boolean {
    const rel = this.dcInput;
    if (!rel || rel.readyState !== "open") return false;
    switch (m.type) {
      case "pointer": {
        const b = new Uint8Array(7);
        const v = new DataView(b.buffer);
        v.setUint8(0, 1);
        v.setUint16(1, m.x, true);
        v.setUint16(3, m.y, true);
        this.moveSeq = (this.moveSeq + 1) & 0xffff;
        v.setUint16(5, this.moveSeq, true);
        const fast = this.dcPointer;
        // Arrastando: vai pelo canal confiável para chegar na ordem certa em relação ao soltar.
        if (this.buttonsDown === 0 && fast && fast.readyState === "open") {
          if (fast.bufferedAmount > 2048) this.heldPointer = b; // rede ocupada: só o mais novo
          else fast.send(b);
        } else {
          rel.send(b);
        }
        return true;
      }
      case "button": {
        const bit = 1 << (m.button & 7);
        this.buttonsDown = m.pressed ? this.buttonsDown | bit : this.buttonsDown & ~bit;
        rel.send(Uint8Array.of(2, m.button & 0xff, m.pressed ? 1 : 0));
        return true;
      }
      case "key": {
        rel.send(Uint8Array.of(3, m.hid & 0xff, (m.hid >> 8) & 0xff, m.pressed ? 1 : 0));
        return true;
      }
      case "scroll": {
        const clamp = (n: number) => Math.max(-127, Math.min(127, n)) & 0xff;
        rel.send(Uint8Array.of(4, clamp(m.dx), clamp(m.dy)));
        return true;
      }
    }
    return false;
  }

  /** Liga o elemento <video> que mostra a mídia do WebRTC. */
  attachVideo(video: HTMLVideoElement) {
    this.video = video;
    this.bindVideo();
  }

  private bindVideo() {
    const v = this.video;
    if (!v || !this.stream) return;
    if (v.srcObject !== this.stream) v.srcObject = this.stream;
    v.muted = false;
    v.play().then(
      () => this.cb?.onAudioBlocked(false),
      () => {
        // Sem permissão para tocar com som: toca sem som e pede um clique.
        v.muted = true;
        v.play().catch(() => {});
        this.cb?.onAudioBlocked(true);
      },
    );
  }

  /** Chamado num clique do usuário para liberar o som. */
  unlockAudio() {
    const v = this.video;
    if (!v) return;
    v.muted = false;
    v.play().then(() => this.cb?.onAudioBlocked(false)).catch(() => {});
  }

  setVolume(volume: number, muted: boolean) {
    this.audio?.setVolume(volume);
    this.audio?.setMuted(muted);
    if (this.video) {
      this.video.volume = Math.min(1, Math.max(0, volume));
      if (muted) this.video.muted = true;
    }
  }

  async rtcStats(): Promise<RtcStats | null> {
    if (!this.pc || this.mode !== "webrtc") return null;
    const report = await this.pc.getStats();
    let fps = 0;
    let bytes = 0;
    let jitter: number | null = null;
    let lost = 0;
    let recv = 0;
    let rtt: number | null = null;
    report.forEach((s: Record<string, unknown> & { type: string }) => {
      if (s.type === "inbound-rtp" && s.kind === "video") {
        fps = Number(s.framesPerSecond ?? 0);
        bytes += Number(s.bytesReceived ?? 0);
        lost = Number(s.packetsLost ?? 0);
        recv = Number(s.packetsReceived ?? 0);
        const jbd = Number(s.jitterBufferDelay ?? 0);
        const jbe = Number(s.jitterBufferEmittedCount ?? 0);
        if (jbe > 0) jitter = (jbd / jbe) * 1000;
      } else if (s.type === "inbound-rtp" && s.kind === "audio") {
        bytes += Number(s.bytesReceived ?? 0);
      } else if (s.type === "candidate-pair" && s.state === "succeeded" && s.nominated) {
        if (s.currentRoundTripTime != null) rtt = Number(s.currentRoundTripTime) * 1000;
      }
    });
    const now = performance.now();
    let kbps = 0;
    let lossPct = 0;
    if (this.lastStats) {
      const dt = (now - this.lastStats.time) / 1000;
      if (dt > 0) kbps = ((bytes - this.lastStats.bytes) * 8) / 1000 / dt;
      const dl = lost - this.lastStats.lost;
      const dr = recv - this.lastStats.recv;
      if (dl + dr > 0) lossPct = (Math.max(0, dl) / (dl + dr)) * 100;
    }
    this.lastStats = { bytes, time: now, lost, recv };
    return { fps, kbps, rttMs: rtt, jitterMs: jitter, lossPct };
  }

  attach(canvas: HTMLCanvasElement, cb: MediaCallbacks) {
    this.canvas = canvas;
    this.ctx = canvas.getContext("2d", { alpha: false });
    this.cb = cb;
    this.tiles = new FrameRenderer(canvas, cb.onResize, cb.onCursor, () => this.requestKeyframe(), cb.onDrawn);
    this.lrd = new LrdDecoder(canvas, cb.onResize, () => this.requestKeyframe());
    const queued = this.pending;
    this.pending = [];
    for (const f of queued) this.route(f);
    cb.onMode(this.mode);
    cancelAnimationFrame(this.raf);
    this.raf = requestAnimationFrame(this.tick);
  }

  detach(canvas: HTMLCanvasElement) {
    if (this.canvas !== canvas) return;
    cancelAnimationFrame(this.raf);
    this.canvas = null;
    this.ctx = null;
    this.tiles = null;
    this.lrd = null;
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
    clearInterval(this.pingTimer);
    this.dcInput = null;
    this.dcPointer = null;
    this.pc?.close();
    this.pc = null;
    if (this.video) this.video.srcObject = null;
    this.stream = null;
  }

  push(data: unknown) {
    if (this.closed) return;
    const bytes = toBytes(data);
    if (!bytes || bytes.length === 0) return;
    if (bytes[0] === 4) {
      if (this.mode !== "webrtc") this.audio?.push(new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength));
      return;
    }
    if (!this.canvas) {
      this.pending.push(bytes);
      if (this.pending.length > 400) this.pending = []; // a tela pede um quadro completo ao abrir
      return;
    }
    this.route(bytes);
  }

  private pushLrd(bytes: Uint8Array) {
    const dec = this.lrd;
    if (!dec) return;
    if (this.lrdBacklog > 90) {
      // A tela daqui não acompanha: descarta e recomeça de um quadro completo.
      this.lrdTimes.resets++;
      dec.reset();
      this.requestKeyframe();
      return;
    }
    this.lrdBacklog++;
    this.lrdBytes += bytes.length;
    const hdr = LrdDecoder.header(bytes);
    const tArrive = performance.now();
    let tStart = 0;
    this.lrdChain = this.lrdChain
      .then(() => {
        tStart = performance.now();
        return dec.apply(bytes);
      })
      .then((info) => {
        if (!info) return;
        this.lrdFrames++;
        const tEnd = performance.now();
        const t = this.lrdTimes;
        t.queueAndApply.push(tEnd - tArrive);
        t.apply.push(tEnd - tStart);
        if (t.apply.length > 300) {
          t.apply.shift();
          t.queueAndApply.shift();
        }
        const off = this.clockOffset();
        if (off != null) {
          const ms = performance.timeOrigin + performance.now() + off - info.captureUs / 1000;
          if (ms >= 0 && ms < 5000) {
            this.glass.push(ms);
            if (this.glass.length > 60) this.glass.shift();
          }
        }
        this.cb?.onDrawn();
      })
      .catch((e) => console.warn("codecLRD", e))
      .finally(() => {
        this.lrdBacklog--;
        // Confirma ao host: ele só manda mais quando a tela daqui acompanha.
        if (hdr) this.sendAck(hdr.frameId);
      });
  }

  private sendAck(frameId: number) {
    const dc = this.dcInput;
    if (!dc || dc.readyState !== "open") return;
    const b = new Uint8Array(5);
    b[0] = 7;
    new DataView(b.buffer).setUint32(1, frameId >>> 0, true);
    dc.send(b);
  }

  private route(bytes: Uint8Array) {
    if (bytes[0] === FRAME_LRD) {
      this.setMode("lrd");
      this.pushLrd(bytes);
      return;
    }
    // Mídia chegando pelo QUIC = modo de reserva (o cursor chega pelo QUIC nos dois modos).
    if ((bytes[0] === 1 || bytes[0] === 3) && this.mode !== "lrd") this.setMode("quic");
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
