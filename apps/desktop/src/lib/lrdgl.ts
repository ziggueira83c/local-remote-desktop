// Renderização do codecLRD na GPU (WebGL2). A tela fica numa textura; cada operação do
// quadro vira um envio direto de pixels para a GPU (texSubImage2D), um recorte com cor
// (scissor + clear) ou uma cópia entre texturas. Nunca há leitura de volta da GPU — que era
// o que travava o Safari ao misturar putImageData e drawImage num canvas 2D.
//
// Convenção: a linha 0 da textura é a linha de cima da tela (só se inverte ao exibir).

const VS = `#version 300 es
in vec2 p;
out vec2 uv;
void main() {
  uv = vec2((p.x + 1.0) * 0.5, (1.0 - p.y) * 0.5);
  gl_Position = vec4(p, 0.0, 1.0);
}`;

const FS = `#version 300 es
precision mediump float;
in vec2 uv;
uniform sampler2D screen;
out vec4 color;
void main() {
  color = vec4(texture(screen, uv).rgb, 1.0);
}`;

export class LrdGl {
  readonly canvas: HTMLCanvasElement;
  private gl: WebGL2RenderingContext;
  private prog: WebGLProgram;
  private screen: WebGLTexture | null = null;
  private scratch: WebGLTexture | null = null;
  private fbScreen: WebGLFramebuffer | null = null;
  private fbScratch: WebGLFramebuffer | null = null;
  width = 0;
  height = 0;

  /** `null` se o navegador não tiver WebGL2. */
  static create(): LrdGl | null {
    const canvas = document.createElement("canvas");
    const gl = canvas.getContext("webgl2", {
      alpha: false,
      antialias: false,
      depth: false,
      stencil: false,
      preserveDrawingBuffer: true,
      desynchronized: true,
    } as WebGLContextAttributes);
    if (!gl) return null;
    try {
      return new LrdGl(canvas, gl);
    } catch (e) {
      console.warn("codecLRD: WebGL2 indisponível", e);
      return null;
    }
  }

  private constructor(canvas: HTMLCanvasElement, gl: WebGL2RenderingContext) {
    this.canvas = canvas;
    this.gl = gl;
    const sh = (type: number, src: string) => {
      const s = gl.createShader(type)!;
      gl.shaderSource(s, src);
      gl.compileShader(s);
      if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s) ?? "shader");
      return s;
    };
    const prog = gl.createProgram()!;
    gl.attachShader(prog, sh(gl.VERTEX_SHADER, VS));
    gl.attachShader(prog, sh(gl.FRAGMENT_SHADER, FS));
    gl.linkProgram(prog);
    if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(prog) ?? "program");
    this.prog = prog;
    const buf = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buf);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
    const loc = gl.getAttribLocation(prog, "p");
    gl.enableVertexAttribArray(loc);
    gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
    gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
  }

  private texture(w: number, h: number): WebGLTexture {
    const gl = this.gl;
    const t = gl.createTexture()!;
    gl.bindTexture(gl.TEXTURE_2D, t);
    gl.texStorage2D(gl.TEXTURE_2D, 1, gl.RGBA8, w, h);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    return t;
  }

  private framebuffer(t: WebGLTexture): WebGLFramebuffer {
    const gl = this.gl;
    const fb = gl.createFramebuffer()!;
    gl.bindFramebuffer(gl.FRAMEBUFFER, fb);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, t, 0);
    return fb;
  }

  resize(w: number, h: number) {
    const gl = this.gl;
    for (const t of [this.screen, this.scratch]) if (t) gl.deleteTexture(t);
    for (const f of [this.fbScreen, this.fbScratch]) if (f) gl.deleteFramebuffer(f);
    this.width = w;
    this.height = h;
    this.canvas.width = w;
    this.canvas.height = h;
    this.screen = this.texture(w, h);
    this.scratch = this.texture(w, h);
    this.fbScreen = this.framebuffer(this.screen);
    this.fbScratch = this.framebuffer(this.scratch);
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
  }

  fill(x: number, y: number, w: number, h: number, r: number, g: number, b: number) {
    const gl = this.gl;
    gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbScreen);
    gl.enable(gl.SCISSOR_TEST);
    gl.scissor(x, y, w, h);
    gl.clearColor(r / 255, g / 255, b / 255, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.disable(gl.SCISSOR_TEST);
  }

  /** Pixels RGB (w*h*3, linha de cima primeiro). */
  rgb(x: number, y: number, w: number, h: number, data: Uint8Array) {
    const gl = this.gl;
    gl.bindTexture(gl.TEXTURE_2D, this.screen);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, x, y, w, h, gl.RGB, gl.UNSIGNED_BYTE, data);
  }

  image(x: number, y: number, bitmap: ImageBitmap) {
    const gl = this.gl;
    gl.bindTexture(gl.TEXTURE_2D, this.screen);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, x, y, gl.RGBA, gl.UNSIGNED_BYTE, bitmap);
  }

  /** Copia (sx,sy,w,h) para (x,y), com regiões sobrepostas (passa pela textura auxiliar). */
  copy(sx: number, sy: number, w: number, h: number, x: number, y: number) {
    const gl = this.gl;
    gl.bindFramebuffer(gl.READ_FRAMEBUFFER, this.fbScreen);
    gl.bindFramebuffer(gl.DRAW_FRAMEBUFFER, this.fbScratch);
    gl.blitFramebuffer(sx, sy, sx + w, sy + h, sx, sy, sx + w, sy + h, gl.COLOR_BUFFER_BIT, gl.NEAREST);
    gl.bindFramebuffer(gl.READ_FRAMEBUFFER, this.fbScratch);
    gl.bindFramebuffer(gl.DRAW_FRAMEBUFFER, this.fbScreen);
    gl.blitFramebuffer(sx, sy, sx + w, sy + h, x, y, x + w, y + h, gl.COLOR_BUFFER_BIT, gl.NEAREST);
    gl.bindFramebuffer(gl.READ_FRAMEBUFFER, null);
    gl.bindFramebuffer(gl.DRAW_FRAMEBUFFER, null);
  }

  /** Mostra a textura da tela no canvas deste renderizador. */
  present() {
    const gl = this.gl;
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    gl.viewport(0, 0, this.width, this.height);
    gl.useProgram(this.prog);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.screen);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
  }
}
