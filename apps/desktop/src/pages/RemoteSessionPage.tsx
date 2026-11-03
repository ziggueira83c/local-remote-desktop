import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api, type AppSettings } from "../lib/api";
import { codeToHid } from "../lib/keymap";
import { FrameRenderer, type CursorState } from "../lib/renderer";
import { attachRenderer, detachRenderer } from "../lib/session";
import { useStore } from "../store";

type ScaleMode = "fit" | "actual";

export function RemoteSessionPage() {
  const outbound = useStore((s) => s.outbound)!;
  const metrics = useStore((s) => s.metrics);
  const local = useStore((s) => s.local);
  const setActiveDisplay = useStore((s) => s.setActiveDisplay);

  const canvasRef = useRef<HTMLCanvasElement>(null);
  const viewportRef = useRef<HTMLDivElement>(null);
  const [frame, setFrame] = useState({ w: 0, h: 0 });
  const [cssSize, setCssSize] = useState({ w: 0, h: 0 });
  const [cursor, setCursor] = useState<CursorState>({ visible: false, x: 0, y: 0 });
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
    });
  }, []);

  // Renderer ligado ao canvas.
  useEffect(() => {
    const canvas = canvasRef.current!;
    const r = new FrameRenderer(
      canvas,
      (w, h) => setFrame({ w, h }),
      setCursor,
      () => api.requestKeyframe().catch(() => {}),
    );
    attachRenderer(r);
    api.requestKeyframe().catch(() => {});
    return () => detachRenderer(r);
  }, [outbound.sessionId]);

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
  const pressedKeys = useRef(new Set<number>());
  const pressedButtons = useRef(new Set<number>());
  const pendingMove = useRef<{ x: number; y: number } | null>(null);
  const rafId = useRef<number | null>(null);
  const wheelAcc = useRef({ x: 0, y: 0 });
  const displayRef = useRef(outbound.activeDisplay);
  displayRef.current = outbound.activeDisplay;

  const releaseAll = useCallback(() => {
    for (const hid of pressedKeys.current) api.sendInput({ type: "key", hid, pressed: false }).catch(() => {});
    for (const b of pressedButtons.current) api.sendInput({ type: "button", button: b, pressed: false }).catch(() => {});
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
    const rect = canvasRef.current!.getBoundingClientRect();
    const nx = Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
    const ny = Math.min(1, Math.max(0, (clientY - rect.top) / rect.height));
    return { x: Math.round(nx * 65535), y: Math.round(ny * 65535) };
  };

  const flushMove = () => {
    rafId.current = null;
    const m = pendingMove.current;
    pendingMove.current = null;
    if (m) api.sendInput({ type: "pointer", displayId: displayRef.current, ...m }).catch(() => {});
  };

  const onPointerMove = (e: React.PointerEvent) => {
    if (!outbound.permissions.controlMouse) return;
    pendingMove.current = toNormalized(e.clientX, e.clientY);
    if (rafId.current === null) rafId.current = requestAnimationFrame(flushMove);
  };

  const onPointerDown = (e: React.PointerEvent) => {
    canvasRef.current?.focus();
    if (!outbound.permissions.controlMouse) return;
    e.preventDefault();
    (e.target as Element).setPointerCapture?.(e.pointerId);
    const button = e.button === 1 ? 1 : e.button === 2 ? 2 : 0;
    // Garante que o ponteiro está no lugar certo antes do clique.
    api.sendInput({ type: "pointer", displayId: displayRef.current, ...toNormalized(e.clientX, e.clientY) }).catch(() => {});
    pressedButtons.current.add(button);
    api.sendInput({ type: "button", button, pressed: true }).catch(() => {});
  };

  const onPointerUp = (e: React.PointerEvent) => {
    if (!outbound.permissions.controlMouse) return;
    const button = e.button === 1 ? 1 : e.button === 2 ? 2 : 0;
    if (!pressedButtons.current.has(button)) return;
    pressedButtons.current.delete(button);
    api.sendInput({ type: "button", button, pressed: false }).catch(() => {});
  };

  // Roda do mouse: precisa de listener não-passivo para impedir a rolagem local.
  useEffect(() => {
    const canvas = canvasRef.current!;
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
        api.sendInput({ type: "scroll", dx, dy }).catch(() => {});
      }
    };
    canvas.addEventListener("wheel", onWheel, { passive: false });
    return () => canvas.removeEventListener("wheel", onWheel);
  }, [outbound.permissions.controlMouse]);

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (!outbound.permissions.controlKeyboard) return;
    if (e.nativeEvent.isComposing) return;
    const hid = codeToHid(e.code, swapActive);
    if (hid === null) return;
    e.preventDefault();
    pressedKeys.current.add(hid);
    // Repetições também são enviadas: o SO remoto não gera auto-repeat para teclas injetadas.
    api.sendInput({ type: "key", hid, pressed: true }).catch(() => {});
  };

  const onKeyUp = (e: React.KeyboardEvent) => {
    if (!outbound.permissions.controlKeyboard) return;
    const hid = codeToHid(e.code, swapActive);
    if (hid === null) return;
    e.preventDefault();
    pressedKeys.current.delete(hid);
    api.sendInput({ type: "key", hid, pressed: false }).catch(() => {});
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
    for (const hid of seq) api.sendInput({ type: "key", hid, pressed: true }).catch(() => {});
    for (const hid of [...seq].reverse()) api.sendInput({ type: "key", hid, pressed: false }).catch(() => {});
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
          <button className="btn btn--small" onClick={() => api.requestKeyframe()} title="Redesenhar a tela inteira">
            Atualizar
          </button>
          <button className="btn btn--small" onClick={toggleFullscreen}>
            Tela cheia
          </button>
        </div>

        <div className="toolbar__group">
          <span className="stats mono small" title="Quadros por segundo · banda · latência">
            {metrics ? `${metrics.fps.toFixed(0)} fps · ${(metrics.kbps / 1000).toFixed(1)} Mbps · ${metrics.rttMs.toFixed(0)} ms` : "—"}
          </span>
          <button className="btn btn--small btn--danger" onClick={() => api.disconnect()}>
            Desconectar
          </button>
        </div>
      </div>

      <div ref={viewportRef} className={mode === "fit" ? "viewport" : "viewport viewport--scroll"}>
        <div className="viewport__stage" style={{ width: cssSize.w || undefined, height: cssSize.h || undefined }}>
          <canvas
            ref={canvasRef}
            className={focused ? "remote remote--focused" : "remote"}
            style={{ width: cssSize.w || undefined, height: cssSize.h || undefined, cursor: "default" }}
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
          />
          {cursor.visible && !canControl && frame.w > 0 && (
            <div
              className="remote-cursor"
              style={{ left: `${(cursor.x / frame.w) * 100}%`, top: `${(cursor.y / frame.h) * 100}%` }}
            />
          )}
          {frame.w === 0 && <div className="viewport__loading">Recebendo a tela…</div>}
        </div>
      </div>
      {canControl && !focused && frame.w > 0 && (
        <div className="hint">Clique na tela remota para enviar mouse e teclado.</div>
      )}
    </div>
  );
}
