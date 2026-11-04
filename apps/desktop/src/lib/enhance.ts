// Melhoria de imagem opcional: nitidez adaptativa (inspirada no AMD FidelityFX CAS) aplicada
// em WebGL na resolução da tela, com interpolação bilinear para ampliar/reduzir.
// Realça bordas e texto sem estourar áreas lisas.

const VERT = `
attribute vec2 pos;
varying vec2 uv;
void main() {
  uv = vec2(pos.x * 0.5 + 0.5, 0.5 - pos.y * 0.5);
  gl_Position = vec4(pos, 0.0, 1.0);
}`;

const FRAG = `
precision mediump float;
uniform sampler2D tex;
uniform vec2 texel;
uniform float sharpness;
varying vec2 uv;
void main() {
  vec3 a = texture2D(tex, uv + vec2(0.0, -texel.y)).rgb;
  vec3 b = texture2D(tex, uv + vec2(-texel.x, 0.0)).rgb;
  vec3 c = texture2D(tex, uv).rgb;
  vec3 d = texture2D(tex, uv + vec2(texel.x, 0.0)).rgb;
  vec3 e = texture2D(tex, uv + vec2(0.0, texel.y)).rgb;
  vec3 mn = min(min(min(a, b), min(d, e)), c);
  vec3 mx = max(max(max(a, b), max(d, e)), c);
  vec3 amp = clamp(min(mn, 1.0 - mx) / max(mx, vec3(0.0001)), 0.0, 1.0);
  amp = sqrt(amp);
  float peak = -1.0 / mix(8.0, 4.5, sharpness);
  vec3 w = amp * peak;
  vec3 col = (c + (a + b + d + e) * w) / (1.0 + 4.0 * w);
  gl_FragColor = vec4(clamp(col, 0.0, 1.0), 1.0);
}`;

export class Enhancer {
  private gl: WebGLRenderingContext;
  private prog: WebGLProgram;
  private tex: WebGLTexture;
  private uTexel: WebGLUniformLocation | null;
  private uSharp: WebGLUniformLocation | null;

  static create(canvas: HTMLCanvasElement): Enhancer | null {
    try {
      const gl = canvas.getContext("webgl", { alpha: false, antialias: false, premultipliedAlpha: false });
      return gl ? new Enhancer(gl) : null;
    } catch {
      return null;
    }
  }

  private constructor(gl: WebGLRenderingContext) {
    this.gl = gl;
    const compile = (type: number, src: string) => {
      const s = gl.createShader(type)!;
      gl.shaderSource(s, src);
      gl.compileShader(s);
      if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s) ?? "shader");
      return s;
    };
    const prog = gl.createProgram()!;
    gl.attachShader(prog, compile(gl.VERTEX_SHADER, VERT));
    gl.attachShader(prog, compile(gl.FRAGMENT_SHADER, FRAG));
    gl.linkProgram(prog);
    if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(prog) ?? "link");
    this.prog = prog;
    gl.useProgram(prog);

    const buf = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buf);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
    const loc = gl.getAttribLocation(prog, "pos");
    gl.enableVertexAttribArray(loc);
    gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);

    this.tex = gl.createTexture()!;
    gl.bindTexture(gl.TEXTURE_2D, this.tex);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    this.uTexel = gl.getUniformLocation(prog, "texel");
    this.uSharp = gl.getUniformLocation(prog, "sharpness");
  }

  /** Desenha `source` (canvas com a tela remota ou o <video> do WebRTC) com nitidez. */
  render(source: HTMLCanvasElement | HTMLVideoElement, strength: number) {
    const srcW = source instanceof HTMLVideoElement ? source.videoWidth : source.width;
    const srcH = source instanceof HTMLVideoElement ? source.videoHeight : source.height;
    const gl = this.gl;
    const canvas = gl.canvas as HTMLCanvasElement;
    const dpr = window.devicePixelRatio || 1;
    const w = Math.max(1, Math.round(canvas.clientWidth * dpr));
    const h = Math.max(1, Math.round(canvas.clientHeight * dpr));
    if (canvas.width !== w || canvas.height !== h) {
      canvas.width = w;
      canvas.height = h;
    }
    if (!srcW || !srcH) return;
    gl.viewport(0, 0, w, h);
    gl.useProgram(this.prog);
    gl.bindTexture(gl.TEXTURE_2D, this.tex);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGB, gl.RGB, gl.UNSIGNED_BYTE, source);
    gl.uniform2f(this.uTexel, 1 / srcW, 1 / srcH);
    gl.uniform1f(this.uSharp, Math.min(1, Math.max(0, strength)));
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
  }
}
