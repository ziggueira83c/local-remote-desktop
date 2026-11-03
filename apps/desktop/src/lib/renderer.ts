// Desenha os quadros binários recebidos (ver crates/lrd-protocol/src/video.rs) num <canvas>.

const FRAME_TILES = 1;
const FRAME_CURSOR = 2;
const FLAG_KEYFRAME = 1;
const MAX_BACKLOG = 12;

export interface CursorState {
  visible: boolean;
  x: number;
  y: number;
}

export class FrameRenderer {
  private ctx: CanvasRenderingContext2D | null = null;
  private chain: Promise<void> = Promise.resolve();
  private backlog = 0;
  private dropping = false;
  width = 0;
  height = 0;

  constructor(
    private canvas: HTMLCanvasElement,
    private onResize: (w: number, h: number) => void,
    private onCursor: (c: CursorState) => void,
    private onOverload: () => void,
  ) {
    this.ctx = canvas.getContext("2d", { alpha: false });
  }

  /** Recebe um quadro em qualquer formato que o canal do Tauri entregar. */
  push(data: unknown) {
    const bytes = toBytes(data);
    if (!bytes || bytes.length < 1) return;
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const type = view.getUint8(0);

    if (type === FRAME_CURSOR && bytes.length >= 6) {
      this.onCursor({ visible: view.getUint8(1) === 1, x: view.getUint16(2, true), y: view.getUint16(4, true) });
      return;
    }
    if (type !== FRAME_TILES) return;

    const keyframe = (view.getUint8(1) & FLAG_KEYFRAME) !== 0;
    if (this.dropping && !keyframe) return; // aguardando um quadro completo
    if (keyframe) this.dropping = false;

    if (this.backlog >= MAX_BACKLOG) {
      // A decodificação não está acompanhando: descarta e pede uma tela completa.
      this.dropping = true;
      this.onOverload();
      return;
    }
    this.backlog++;
    this.chain = this.chain
      .then(() => this.draw(bytes, view))
      .catch((e) => console.warn("falha ao desenhar quadro", e))
      .finally(() => {
        this.backlog--;
      });
  }

  private async draw(bytes: Uint8Array, view: DataView) {
    const width = view.getUint16(2, true);
    const height = view.getUint16(4, true);
    const count = view.getUint16(6, true);
    if (width !== this.width || height !== this.height) {
      this.width = width;
      this.height = height;
      this.canvas.width = width;
      this.canvas.height = height;
      this.onResize(width, height);
    }

    const tiles: { x: number; y: number; img: Promise<ImageBitmap> }[] = [];
    let off = 12;
    for (let i = 0; i < count; i++) {
      const x = view.getUint16(off, true);
      const y = view.getUint16(off + 2, true);
      const len = view.getUint32(off + 8, true);
      off += 12;
      const jpeg = bytes.subarray(off, off + len);
      off += len;
      tiles.push({ x, y, img: createImageBitmap(new Blob([jpeg as unknown as BlobPart], { type: "image/jpeg" })) });
    }
    const ctx = this.ctx;
    for (const t of tiles) {
      const img = await t.img;
      ctx?.drawImage(img, t.x, t.y);
      img.close();
    }
  }
}

function toBytes(data: unknown): Uint8Array | null {
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (data instanceof Uint8Array) return data;
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  if (Array.isArray(data)) return Uint8Array.from(data as number[]);
  return null;
}
