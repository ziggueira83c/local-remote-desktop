// Sincronização de áudio e vídeo.
//
// Host e controlador têm relógios diferentes. A diferença entre eles (`offset`) é estimada
// pelo menor atraso observado (local - carimbo do host). Cada mídia tem um atraso extra
// próprio (decodificar vídeo demora mais que receber áudio); o atraso de apresentação é o
// maior dos dois mais uma folga, e as duas mídias são apresentadas nesse mesmo instante.

export class AvSync {
  private offset = Number.POSITIVE_INFINITY;
  private lastObs = 0;
  private videoLag = 0;
  private audioLag = 0;
  private lastAudio = -1e9;

  private observe(tsUs: number, localMs: number): number {
    const d = localMs - tsUs / 1000;
    const dt = this.lastObs ? Math.max(0, localMs - this.lastObs) : 0;
    this.lastObs = localMs;
    // Mínimo com relaxamento lento (acompanha a deriva entre os relógios).
    this.offset = Math.min(d, this.offset + dt * 0.0005);
    return Math.max(0, d - this.offset);
  }

  noteVideo(tsUs: number, localMs: number) {
    const lag = this.observe(tsUs, localMs);
    this.videoLag = Math.max(lag, this.videoLag * 0.99 + lag * 0.01);
  }

  noteAudio(tsUs: number, localMs: number) {
    const lag = this.observe(tsUs, localMs);
    this.audioLag = Math.max(lag, this.audioLag * 0.99 + lag * 0.01);
    this.lastAudio = localMs;
  }

  /** Há áudio chegando? Sem áudio, o vídeo é mostrado assim que decodificado. */
  audioActive(now = performance.now()): boolean {
    return now - this.lastAudio < 1000;
  }

  /** Atraso de apresentação em ms. */
  targetDelay(): number {
    return Math.min(400, Math.max(30, Math.max(this.videoLag, this.audioLag) + 15));
  }

  /** Instante (em performance.now()) em que a amostra com carimbo `tsUs` deve aparecer/tocar. */
  playoutMs(tsUs: number): number {
    if (!Number.isFinite(this.offset)) return performance.now();
    return tsUs / 1000 + this.offset + this.targetDelay();
  }
}
