// Reprodução do áudio PCM recebido, agendada pelo relógio comum (ver sync.ts).
import type { AvSync } from "./sync";

export class AudioPlayer {
  private gain: GainNode;
  private nextTime = 0;
  private volume = 1;
  private muted = false;

  constructor(
    private ctx: AudioContext,
    private sync: AvSync,
  ) {
    this.gain = ctx.createGain();
    this.gain.connect(ctx.destination);
  }

  setVolume(v: number) {
    this.volume = Math.min(1, Math.max(0, v));
    this.apply();
  }

  setMuted(m: boolean) {
    this.muted = m;
    this.apply();
  }

  private apply() {
    this.gain.gain.value = this.muted ? 0 : this.volume;
  }

  /** `view` aponta para o quadro inteiro (cabeçalho de 14 bytes + PCM i16 LE intercalado). */
  push(view: DataView) {
    const channels = view.getUint8(1);
    const rate = view.getUint32(2, true);
    const tsUs = Number(view.getBigUint64(6, true));
    const nowMs = performance.now();
    this.sync.noteAudio(tsUs, nowMs);
    if (this.ctx.state === "suspended") this.ctx.resume().catch(() => {});

    const frames = Math.floor((view.byteLength - 14) / (2 * channels));
    if (frames <= 0 || rate < 8000) return;
    const outCh = Math.min(2, channels);
    const buffer = this.ctx.createBuffer(outCh, frames, rate);
    for (let c = 0; c < outCh; c++) {
      const data = buffer.getChannelData(c);
      for (let i = 0; i < frames; i++) {
        data[i] = view.getInt16(14 + (i * channels + c) * 2, true) / 32768;
      }
    }

    const outputLatency =
      ((this.ctx as AudioContext & { outputLatency?: number }).outputLatency ?? 0) + (this.ctx.baseLatency ?? 0);
    let when = this.ctx.currentTime + (this.sync.playoutMs(tsUs) - nowMs) / 1000 - outputLatency;
    const earliest = this.ctx.currentTime + 0.005;
    if (when < earliest) {
      if (earliest - when > 0.2) {
        this.nextTime = 0; // atrasado demais: descarta para não acumular atraso
        return;
      }
      when = earliest;
    }
    // Encosta blocos consecutivos para evitar estalos.
    if (this.nextTime && Math.abs(when - this.nextTime) < 0.03) when = Math.max(this.nextTime, earliest);
    const src = this.ctx.createBufferSource();
    src.buffer = buffer;
    src.connect(this.gain);
    src.start(when);
    this.nextTime = when + buffer.duration;
  }

  close() {
    this.gain.disconnect();
  }
}
