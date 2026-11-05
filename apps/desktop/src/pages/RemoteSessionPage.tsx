import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api, type AppSettings } from "../lib/api";
import { codeToHid } from "../lib/keymap";
import { Enhancer } from "../lib/enhance";
import type { InputMsg, LatencyStats, MediaMode, RtcStats } from "../lib/media";
import { CURSOR_CSS, type CursorState } from "../lib/renderer";
import { currentEngine } from "../lib/session";
import { useStore } from "../store";

type ScaleMode = "fit" | "actual";
type FrameMeta = { rtpTimestamp?: number; expectedDisplayTime?: number; presentationTime?: number };

export function RemoteSessionPage() {
  const outbound = useStore((s) => s.outbound)!;
  const metrics = useStore((s) => s.metrics);
  const local = useStore((s) => s.local);
  const setActiveDisplay = useStore((s) => s.setActiveDisplay);

  const canvasRef = useRef<HTMLCanvasElement>(null);
  const videoRef = useRef<HTMLVideoElement>(null);
  const stageRef = useRef<HTMLDivElement>(null);
  const [mediaMode, setMediaMode] = useState<MediaMode>("pending");
  const mediaModeRef = useRef<MediaMode>("pending");
  mediaModeRef.current = mediaMode;
  const [audioBlocked, setAudioBlocked] = useState(false);
  const [rtc, setRtc] = useState<RtcStats | null>(null);
  const [lat, setLat] = useState<LatencyStats | null>(null);
  const [lrdStats, setLrdStats] = useState<{ fps: number; kbps: number } | null>(null);
  const [lrdVideo, setLrdVideo] = useState<string | null>(null);
  const overlayRef = useRef<HTMLCanvasElement>(null);
  const enhancerRef = useRef<Enhancer | null>(null);
  const [enhance, setEnhance] = useState(false);
  const [strength, setStrength] = useState(0.5);
  const enhanceRef = useRef({ on: false, strength: 0.5 });
  enhanceRef.current = { on: enhance, strength };
  const [audioOn, setAudioOn] = useState(outbound.audioEnabled);
  const [volume, setVolume] = useState(1);
  const viewportRef = useRef<HTMLDivElement>(null);
  const [frame, setFrame] = useState({ w: 0, h: 0 });
  const [cssSize, setCssSize] = useState({ w: 0, h: 0 });
  const [cursor, setCursor] = useState<CursorState>({ visible: true, x: 0, y: 0, kind: 0 });
  const hostStats = useStore((s) => s.hostStats);
  const [showDiag, setShowDiag] = useState(false);
  const [mode, setMode] = useState<ScaleMode>("fit");
  const [quality, setQuality] = useState<AppSettings["qualityProfile"]>("balanced");
  const [fps, setFps] = useState(30);
  const [swap, setSwap] = useState(true);
  const [focused, setFocused] = useState(false);

  const canControl = outbound.permissions.controlMouse || outbound.permissions.controlKeyboard;
  const crossPlatform = !!local && local.platform !== outbound.remotePlatform;
  const swapActive = swap && crossPlatform;

  // Preferências iniciais vêm das configurações.
  useEffect(() => {
    api.getSettings().then((s) => {
      setQuality(s.qualityProfile);
      setFps(s.maxFps);
      setSwap(s.swapCtrlCmd);
      setEnhance(s.enhanceImage);
      setStrength(s.enhanceStrength);
    });
  }, []);

  // Motor de mídia ligado ao canvas.
  useEffect(() => {
    const canvas = canvasRef.current!;
    const engine = currentEngine();
    if (!engine) return;
    engine.attach(canvas, {
      onMode: setMediaMode,
      onAudioBlocked: setAudioBlocked,
      onResize: (w, h) => {
        if (mediaModeRef.current !== "webrtc") setFrame({ w, h });
      },
      onCursor: setCursor,
      onKeyframeNeeded: () => api.requestKeyframe().catch(() => {}),
      onDrawn: () => {
        const e = enhanceRef.current;
        if (e.on && enhancerRef.current) enhancerRef.current.render(canvas, e.strength);
      },
    });
    if (videoRef.current) engine.attachVideo(videoRef.current);
    api.requestKeyframe().catch(() => {});
    return () => engine.detach(canvas);
  }, [outbound.sessionId]);

  // WebRTC: o tamanho vem do <video>; a melhoria de imagem acompanha cada quadro novo.
  useEffect(() => {
    const v = videoRef.current;
    if (!v || mediaMode !== "webrtc") return;
    const onSize = () => {
      if (v.videoWidth && v.videoHeight) setFrame({ w: v.videoWidth, h: v.videoHeight });
    };
    onSize();
    v.addEventListener("resize", onSize);
    v.addEventListener("loadedmetadata", onSize);
    let stop = false;
    const vfc = v as HTMLVideoElement & {
      requestVideoFrameCallback?: (cb: (now: number, meta: FrameMeta) => void) => number;
    };
    const step = (_now?: number, meta?: FrameMeta) => {
      if (stop) return;
      if (meta) currentEngine()?.noteVideoFrame(meta);
      const e = enhanceRef.current;
      if (e.on && enhancerRef.current) enhancerRef.current.render(v, e.strength);
      if (vfc.requestVideoFrameCallback) vfc.requestVideoFrameCallback(step);
      else requestAnimationFrame(step);
    };
    step();
    return () => {
      stop = true;
      v.removeEventListener("resize", onSize);
      v.removeEventListener("loadedmetadata", onSize);
    };
  }, [mediaMode]);

  // Estatísticas do WebRTC (FPS, banda, latência, perdas) a cada segundo.
  useEffect(() => {
    if (mediaMode !== "webrtc" && mediaMode !== "lrd") {
      setRtc(null);
      return;
    }
    const t = setInterval(() => {
      const engine = currentEngine();
      engine
        ?.rtcStats()
        .then(setRtc)
        .catch(() => {});
      setLat(engine?.latency() ?? null);
      setLrdStats(engine?.lrdStats() ?? null);
      setLrdVideo(engine?.lrdVideoCodec() ?? null);
    }, 1000);
    return () => clearInterval(t);
  }, [mediaMode]);

  // Melhoria de imagem: cria o WebGL sob demanda e redesenha ao mudar a intensidade.
  useEffect(() => {
    if (enhance && !enhancerRef.current && overlayRef.current) {
      enhancerRef.current = Enhancer.create(overlayRef.current);
      if (!enhancerRef.current) setEnhance(false);
    }
    const src = mediaMode === "webrtc" ? videoRef.current : canvasRef.current;
    if (enhance && enhancerRef.current && src) {
      enhancerRef.current.render(src, strength);
    }
  }, [enhance, strength, cssSize, mediaMode]);

  // Áudio: volume local e liga/desliga a transmissão no host.
  useEffect(() => {
    currentEngine()?.setVolume(volume, !audioOn);
  }, [audioOn, volume, mediaMode]);

  const toggleAudio = () => {
    const next = !audioOn;
    setAudioOn(next);
    api.setAudio(next).catch(() => {});
    if (next) currentEngine()?.unlockAudio();
  };

  // Tamanho exibido: "ajustar" mantém a proporção; "100%" mostra 1 pixel remoto por pixel físico.
  const recompute = useCallback(() => {
    const vp = viewportRef.current;
    if (!vp || !frame.w || !frame.h) return;
    if (mode === "actual") {
      const dpr = window.devicePixelRatio || 1;
      setCssSize({ w: frame.w / dpr, h: frame.h / dpr });
    } else {
      const s = Math.min(vp.clientWidth / frame.w, vp.clientHeight / frame.h);
      setCssSize({ w: Math.floor(frame.w * s), h: Math.floor(frame.h * s) });
    }
  }, [frame, mode]);

  useLayoutEffect(() => {
    recompute();
    const vp = viewportRef.current;
    if (!vp) return;
    const ro = new ResizeObserver(recompute);
    ro.observe(vp);
    return () => ro.disconnect();
  }, [recompute]);

  // ---------- entrada ----------
  // Caminho rápido: canal de dados do WebRTC direto ao host. Reserva: comando do app (QUIC).
  const sendInput = (m: InputMsg) => {
    if (currentEngine()?.sendInput(m)) return;
    api.sendInput({ ...m, displayId: displayRef.current } as Parameters<typeof api.sendInput>[0]).catch(() => {});
  };
  const pressedKeys = useRef(new Set<number>());
  const pressedButtons = useRef(new Set<number>());
  const pendingMove = useRef<{ x: number; y: number } | null>(null);
  const moveTimer = useRef<number | null>(null);
  const lastMoveSent = useRef(0);
  const wheelAcc = useRef({ x: 0, y: 0 });
  const displayRef = useRef(outbound.activeDisplay);
  displayRef.current = outbound.activeDisplay;

  const releaseAll = useCallback(() => {
    for (const hid of pressedKeys.current) sendInput({ type: "key", hid, pressed: false });
    for (const b of pressedButtons.current) sendInput({ type: "button", button: b, pressed: false });
    pressedKeys.current.clear();
    pressedButtons.current.clear();
  }, []);

  useEffect(() => {
    const onBlur = () => releaseAll();
    window.addEventListener("blur", onBlur);
    return () => {
      window.removeEventListener("blur", onBlur);
      releaseAll();
    };
  }, [releaseAll]);

  const toNormalized = (clientX: number, clientY: number) => {
    const rect = stageRef.current!.getBoundingClientRect();
    const nx = Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
    const ny = Math.min(1, Math.max(0, (clientY - rect.top) / rect.height));
    return { x: Math.round(nx * 65535), y: Math.round(ny * 65535) };
  };

  // Movimento do mouse enviado na hora (até ~500 vezes/s), sem esperar o próximo quadro da tela.
  const MOVE_INTERVAL_MS = 2;
  const flushMove = () => {
    moveTimer.current = null;
    const m = pendingMove.current;
    pendingMove.current = null;
    if (!m) return;
    lastMoveSent.current = performance.now();
    sendInput({ type: "pointer", ...m });
  };

  const handleMove = (clientX: number, clientY: number) => {
    if (!outbound.permissions.controlMouse) return;
    pendingMove.current = toNormalized(clientX, clientY);
    const since = performance.now() - lastMoveSent.current;
    if (since >= MOVE_INTERVAL_MS) {
      if (moveTimer.current !== null) {
        clearTimeout(moveTimer.current);
        moveTimer.current = null;
      }
      flushMove();
    } else if (moveTimer.current === null) {
      moveTimer.current = window.setTimeout(flushMove, MOVE_INTERVAL_MS - since);
    }
  };
  const handleMoveRef = useRef(handleMove);
  handleMoveRef.current = handleMove;

  // `pointerrawupdate` (Chromium/WebView2) entrega o movimento assim que o mouse informa,
  // sem esperar o próximo quadro da interface; nos outros, `pointermove`.
  const rawSupported = typeof window !== "undefined" && "onpointerrawupdate" in window;
  useEffect(() => {
    if (!rawSupported) return;
    const stage = stageRef.current!;
    const onRaw = (e: Event) => {
      const p = e as PointerEvent;
      handleMoveRef.current(p.clientX, p.clientY);
    };
    stage.addEventListener("pointerrawupdate", onRaw);
    return () => stage.removeEventListener("pointerrawupdate", onRaw);
  }, [rawSupported]);

  const onPointerMove = (e: React.PointerEvent) => {
    if (rawSupported) return;
    handleMove(e.clientX, e.clientY);
  };

  const onPointerDown = (e: React.PointerEvent) => {
    stageRef.current?.focus();
    if (audioBlocked && audioOn) currentEngine()?.unlockAudio();
    if (!outbound.permissions.controlMouse) return;
    e.preventDefault();
    (e.target as Element).setPointerCapture?.(e.pointerId);
    const button = e.button === 1 ? 1 : e.button === 2 ? 2 : 0;
    // Garante que o ponteiro está no lugar certo antes do clique.
    sendInput({ type: "pointer", ...toNormalized(e.clientX, e.clientY) });
    pressedButtons.current.add(button);
    sendInput({ type: "button", button, pressed: true });
  };

  const onPointerUp = (e: React.PointerEvent) => {
    if (!outbound.permissions.controlMouse) return;
    const button = e.button === 1 ? 1 : e.button === 2 ? 2 : 0;
    if (!pressedButtons.current.has(button)) return;
    pressedButtons.current.delete(button);
    sendInput({ type: "button", button, pressed: false });
  };

  // Roda do mouse: precisa de listener não-passivo para impedir a rolagem local.
  useEffect(() => {
    const stage = stageRef.current!;
    const onWheel = (e: WheelEvent) => {
      if (!outbound.permissions.controlMouse) return;
      e.preventDefault();
      const unit = e.deltaMode === 1 ? 1 / 3 : e.deltaMode === 2 ? 1 : 1 / 100;
      wheelAcc.current.x += e.deltaX * unit;
      wheelAcc.current.y += e.deltaY * unit;
      const dx = Math.trunc(wheelAcc.current.x);
      const dy = Math.trunc(wheelAcc.current.y);
      if (dx || dy) {
        wheelAcc.current.x -= dx;
        wheelAcc.current.y -= dy;
        sendInput({ type: "scroll", dx, dy });
      }
    };
    stage.addEventListener("wheel", onWheel, { passive: false });
    return () => stage.removeEventListener("wheel", onWheel);
  }, [outbound.permissions.controlMouse]);

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (!outbound.permissions.controlKeyboard) return;
    if (e.nativeEvent.isComposing) return;
    const hid = codeToHid(e.code, swapActive);
    if (hid === null) return;
    e.preventDefault();
    pressedKeys.current.add(hid);
    // Repetições também são enviadas: o SO remoto não gera auto-repeat para teclas injetadas.
    sendInput({ type: "key", hid, pressed: true });
  };

  const onKeyUp = (e: React.KeyboardEvent) => {
    if (!outbound.permissions.controlKeyboard) return;
    const hid = codeToHid(e.code, swapActive);
    if (hid === null) return;
    e.preventDefault();
    pressedKeys.current.delete(hid);
    sendInput({ type: "key", hid, pressed: false });
  };

  // ---------- barra de ferramentas ----------
  const changeDisplay = (id: string) => {
    releaseAll();
    setActiveDisplay(id);
    api.selectDisplay(id).catch(() => {});
  };
  const changeQuality = (q: AppSettings["qualityProfile"], f: number) => {
    setQuality(q);
    setFps(f);
    api.setVideoQuality(q, f).catch(() => {});
  };
  const toggleFullscreen = async () => {
    const win = getCurrentWindow();
    try {
      await win.setFullscreen(!(await win.isFullscreen()));
    } catch {
      /* sem permissão ou não suportado */
    }
  };
  const sendTaskManager = () => {
    // Ctrl+Alt+Del real não pode ser injetado no Windows (secure desktop); envia Ctrl+Shift+Esc
    // (Gerenciador de Tarefas) como alternativa útil.
    const seq = [0xe0, 0xe1, 0x29];
    for (const hid of seq) sendInput({ type: "key", hid, pressed: true });
    for (const hid of [...seq].reverse()) sendInput({ type: "key", hid, pressed: false });
  };

  return (
    <div className="session">
      <div className="toolbar">
        <div className="toolbar__group">
          <span className="toolbar__title">{outbound.remoteName}</span>
          <span className={canControl ? "pill pill--ok" : "pill"}>{canControl ? "Controle" : "Somente visualização"}</span>
        </div>

        <div className="toolbar__group">
          {outbound.displays.length > 1 && (
            <select
              value={outbound.activeDisplay}
              onChange={(e) => changeDisplay(e.target.value)}
              disabled={!outbound.permissions.switchDisplays}
              title="Monitor"
            >
              {outbound.displays.map((d, i) => (
                <option key={d.id} value={d.id}>
                  Monitor {i + 1}
                  {d.primary ? " (principal)" : ""} · {d.width}×{d.height}
                </option>
              ))}
            </select>
          )}
          <select value={quality} onChange={(e) => changeQuality(e.target.value as AppSettings["qualityProfile"], fps)} title="Qualidade">
            <option value="speed">Fluidez</option>
            <option value="balanced">Equilibrado</option>
            <option value="quality">Nitidez</option>
          </select>
          <select value={fps} onChange={(e) => changeQuality(quality, Number(e.target.value))} title="Quadros por segundo">
            <option value={15}>15 FPS</option>
            <option value={30}>30 FPS</option>
            <option value={60}>60 FPS</option>
          </select>
          <button className="btn btn--small" onClick={() => setMode(mode === "fit" ? "actual" : "fit")}>
            {mode === "fit" ? "100%" : "Ajustar"}
          </button>
          {crossPlatform && (
            <label className="check" title="Converte Ctrl em Cmd (e vice-versa) entre Windows e Mac">
              <input type="checkbox" checked={swap} onChange={(e) => setSwap(e.target.checked)} />
              Ctrl ⇄ Cmd
            </label>
          )}
          {outbound.remotePlatform === "windows" && outbound.permissions.controlKeyboard && (
            <button className="btn btn--small" onClick={sendTaskManager} title="Abre o Gerenciador de Tarefas remoto">
              Ctrl+Shift+Esc
            </button>
          )}
          <button
            className={audioOn ? "btn btn--small btn--on" : "btn btn--small"}
            onClick={toggleAudio}
            title={audioOn ? "Desligar o áudio do outro computador" : "Ligar o áudio do outro computador"}
          >
            {audioOn ? "🔊 Áudio" : "🔇 Áudio"}
          </button>
          {audioOn && (
            <input
              className="slider"
              type="range"
              min={0}
              max={1}
              step={0.05}
              value={volume}
              onChange={(e) => setVolume(Number(e.target.value))}
              title="Volume"
            />
          )}
          <label className="check" title="Nitidez adaptativa aplicada na sua tela (não muda o que é transmitido)">
            <input type="checkbox" checked={enhance} onChange={(e) => setEnhance(e.target.checked)} />
            Melhorar imagem
          </label>
          {enhance && (
            <select value={strength} onChange={(e) => setStrength(Number(e.target.value))} title="Intensidade">
              <option value={0.25}>Leve</option>
              <option value={0.5}>Média</option>
              <option value={0.85}>Forte</option>
            </select>
          )}
          <button className="btn btn--small" onClick={() => api.requestKeyframe()} title="Redesenhar a tela inteira">
            Atualizar
          </button>
          <button className="btn btn--small" onClick={toggleFullscreen}>
            Tela cheia
          </button>
        </div>

        <div className="toolbar__group">
          <span className="stats mono small" title="Quadros por segundo · banda · latência">
            {mediaMode === "webrtc"
              ? rtc
                ? `WebRTC · ${rtc.fps.toFixed(0)} fps · ${(rtc.kbps / 1000).toFixed(1)} Mbps · ${(rtc.rttMs ?? metrics?.rttMs ?? 0).toFixed(0)} ms${lat?.glassMs != null ? ` · atraso ${lat.glassMs.toFixed(0)} ms` : ""}${rtc.lossPct >= 0.5 ? ` · perda ${rtc.lossPct.toFixed(1)}%` : ""}`
                : "WebRTC"
              : mediaMode === "lrd"
                ? `codecLRD${lrdVideo ? ` + vídeo ${lrdVideo.startsWith("hvc1") ? "H.265" : "H.264"}` : ""} · ${lrdStats ? `${lrdStats.fps.toFixed(0)} fps · ${(lrdStats.kbps / 1000).toFixed(1)} Mbps` : "—"}${lat?.glassMs != null ? ` · atraso ${lat.glassMs.toFixed(0)} ms` : ""}`
              : `${mediaMode === "quic" ? (outbound.videoCodec === "h264" ? "H.264" : "JPEG") : "Conectando mídia"} · ${
                  metrics ? `${metrics.fps.toFixed(0)} fps · ${(metrics.kbps / 1000).toFixed(1)} Mbps · ${metrics.rttMs.toFixed(0)} ms` : "—"
                }`}
          </span>
          <button
            className={showDiag ? "btn btn--small btn--on" : "btn btn--small"}
            onClick={() => setShowDiag(!showDiag)}
            title="Diagnóstico de desempenho"
          >
            📊
          </button>
          <button className="btn btn--small btn--danger" onClick={() => api.disconnect()}>
            Desconectar
          </button>
        </div>
      </div>

      <div ref={viewportRef} className={mode === "fit" ? "viewport" : "viewport viewport--scroll"}>
        <div
          ref={stageRef}
          className={focused ? "viewport__stage stage--focused" : "viewport__stage"}
          style={{
            width: cssSize.w || undefined,
            height: cssSize.h || undefined,
            // Cursor local com o formato do cursor remoto: responde na hora, sem esperar a imagem.
            cursor: outbound.permissions.controlMouse
              ? cursor.visible
                ? CURSOR_CSS[cursor.kind] ?? "default"
                : "none"
              : "default",
          }}
          tabIndex={0}
          onFocus={() => setFocused(true)}
          onBlur={() => {
            setFocused(false);
            releaseAll();
          }}
          onPointerMove={onPointerMove}
          onPointerDown={onPointerDown}
          onPointerUp={onPointerUp}
          onContextMenu={(e) => e.preventDefault()}
          onKeyDown={onKeyDown}
          onKeyUp={onKeyUp}
        >
          <canvas
            ref={canvasRef}
            className="remote"
            style={{
              width: cssSize.w || undefined,
              height: cssSize.h || undefined,
              display: mediaMode === "webrtc" ? "none" : "block",
            }}
          />
          <video
            ref={videoRef}
            className="remote"
            autoPlay
            playsInline
            disablePictureInPicture
            style={{
              width: cssSize.w || undefined,
              height: cssSize.h || undefined,
              display: mediaMode === "webrtc" ? "block" : "none",
            }}
          />
          <canvas
            ref={overlayRef}
            className="remote-overlay"
            style={{
              width: cssSize.w || undefined,
              height: cssSize.h || undefined,
              visibility: enhance ? "visible" : "hidden",
            }}
          />
          {cursor.visible && !canControl && frame.w > 0 && (
            <div
              className="remote-cursor"
              style={{ left: `${(cursor.x / frame.w) * 100}%`, top: `${(cursor.y / frame.h) * 100}%` }}
            />
          )}
          {frame.w === 0 && <div className="viewport__loading">Recebendo a tela…</div>}
          {audioBlocked && audioOn && mediaMode === "webrtc" && (
            <button
              className="btn btn--small audio-unlock"
              onPointerDown={(e) => e.stopPropagation()}
              onClick={() => currentEngine()?.unlockAudio()}
            >
              🔊 Clique para ativar o som
            </button>
          )}
        </div>
      </div>
      {showDiag && (
        <div className="diag mono small">
          <div className="diag__title">Diagnóstico</div>
          <div>Transporte: {hostStats?.transport === "webrtc" ? "WebRTC" : hostStats?.transport === "quic" ? "QUIC (reserva)" : "negociando"}</div>
          <div>Captura: {hostStats ? `${hostStats.captureSource} · ${hostStats.captureFps.toFixed(0)} quadros/s` : "—"}</div>
          <div>Codificador: {hostStats?.encoder ?? "—"}</div>
          <div>
            Codificação: {hostStats ? `${hostStats.encodeFps.toFixed(0)} fps · ${hostStats.encodeMs.toFixed(1)} ms/quadro` : "—"}
          </div>
          <div>Resolução enviada: {hostStats ? `${hostStats.width}×${hostStats.height}` : "—"}</div>
          <div>Taxa alvo: {hostStats ? `${(hostStats.bitrateKbps / 1000).toFixed(1)} Mbps` : "—"}</div>
          {lat && (
            <>
              <div>
                Atraso real (captura → tela): {lat.glassMs != null ? `${lat.glassMs.toFixed(0)} ms` : "medindo…"}
              </div>
              <div>
                Comandos: {lat.directInput ? "canal direto" : "caminho de reserva"}
                {lat.inputRttMs != null ? ` · ${lat.inputRttMs.toFixed(0)} ms ida e volta` : ""}
              </div>
            </>
          )}
          {rtc && (
            <>
              <div>Recebido: {`${rtc.fps.toFixed(0)} fps · ${(rtc.kbps / 1000).toFixed(1)} Mbps`}</div>
              <div>
                Rede: {`${(rtc.rttMs ?? 0).toFixed(0)} ms ida e volta · perda ${rtc.lossPct.toFixed(1)}%`}
                {rtc.jitterMs != null ? ` · buffer ${rtc.jitterMs.toFixed(0)} ms` : ""}
              </div>
            </>
          )}
          <div className="diag__hint">
            {hostStats && hostStats.captureFps < 20 && hostStats.captureSource === "cópia"
              ? "A captura está no modo por cópia (lento). A captura contínua não iniciou nesse computador."
              : lat?.glassMs != null && lat.glassMs > 120 && rtc && rtc.lossPct < 1
                ? "O atraso está alto sem perda na rede: o computador acessado pode estar sobrecarregado."
                : hostStats && hostStats.encodeMs > 25
                ? "A codificação está lenta: o codificador de hardware não está ativo."
                : rtc && rtc.lossPct > 2
                  ? "Há perda de pacotes na rede: prefira cabo ou Wi‑Fi 5 GHz."
                  : ""}
          </div>
        </div>
      )}
      {canControl && !focused && frame.w > 0 && (
        <div className="hint">Clique na tela remota para enviar mouse e teclado.</div>
      )}
    </div>
  );
}
