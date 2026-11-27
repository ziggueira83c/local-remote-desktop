// Inicia conexões e mantém o motor de mídia da sessão (vídeo + áudio).
import { Channel } from "@tauri-apps/api/core";
import { api, errorMessage, type Profile } from "./api";
import { MediaEngine, supportedCodecs } from "./media";
import { useStore } from "../store";

let engine: MediaEngine | null = null;
let audioCtx: AudioContext | null = null;

/** Motor de mídia da sessão atual (a tela de sessão liga o canvas nele). */
export function currentEngine(): MediaEngine | null {
  return engine;
}

export function endMedia() {
  engine?.close();
  engine = null;
}

export interface ConnectTarget {
  address: string;
  port?: number;
  expectedFingerprint?: string;
  label: string;
  password?: string;
  rememberPassword?: boolean;
}

// Última conexão bem-sucedida (só em memória), para reconectar se a rede cair.
let lastTarget: { target: ConnectTarget; profile: Profile } | null = null;
let reconnectRun = 0;

/** Conecta; devolve `true` se a sessão abriu. `silent`: sem avisos (reconexão automática). */
export async function connectTo(target: ConnectTarget, profile: Profile, opts: { silent?: boolean } = {}): Promise<boolean> {
  const { setProgress, toast, progress, outbound } = useStore.getState();
  if (progress || outbound) {
    if (!opts.silent) toast("warning", "Já existe uma conexão em andamento.");
    return false;
  }
  if (!opts.silent) cancelReconnect();
  // O AudioContext precisa nascer dentro do clique do usuário (política de autoplay).
  try {
    if (!audioCtx) {
      const Ctor = window.AudioContext ?? (window as unknown as { webkitAudioContext: typeof AudioContext }).webkitAudioContext;
      audioCtx = new Ctor({ latencyHint: "interactive" });
    }
    audioCtx.resume().catch(() => {});
  } catch {
    audioCtx = null;
  }

  setProgress({ state: "connecting", target: target.label, remoteName: null, fingerprint: null, sas: null });
  endMedia();
  engine = new MediaEngine(audioCtx);
  const current = engine;
  const channel = new Channel<ArrayBuffer>();
  channel.onmessage = (data) => current.push(data);

  let receiveAudio = true;
  try {
    receiveAudio = (await api.getSettings()).receiveAudio;
  } catch {
    /* padrão */
  }
  const codecs = await supportedCodecs();

  try {
    await api.connect(
      {
        address: target.address,
        port: target.port,
        expectedFingerprint: target.expectedFingerprint,
        profile,
        codecs,
        audio: receiveAudio && !!audioCtx,
        password: target.password || undefined,
        rememberPassword: !!target.rememberPassword,
      },
      channel,
    );
    lastTarget = { target: { ...target, rememberPassword: false }, profile };
    return true;
  } catch (e) {
    useStore.getState().setProgress(null);
    if (engine === current) endMedia();
    const msg = errorMessage(e);
    if (!opts.silent && !/cancel|closed_by_user|encerrada antes/i.test(msg)) toast("error", msg);
    return false;
  }
}

const RECONNECT_FOR_MS = 5 * 60_000;
const RECONNECT_EVERY_MS = 3000;

/**
 * A conexão caiu (rede): se a opção estiver ligada, tenta reconectar a cada 3 s por até
 * 5 minutos, mostrando "Reconectando…".
 */
export async function reconnectAfterLoss(reason: string) {
  const { toast, setReconnecting } = useStore.getState();
  const last = lastTarget;
  let enabled = true;
  try {
    enabled = (await api.getSettings()).autoReconnect;
  } catch {
    /* padrão: ligado */
  }
  if (!last || !enabled) {
    toast("info", reason);
    return;
  }
  const run = ++reconnectRun;
  const started = Date.now();
  let attempt = 0;
  while (run === reconnectRun && Date.now() - started < RECONNECT_FOR_MS) {
    attempt++;
    setReconnecting({ label: last.target.label, attempt });
    if (await connectTo(last.target, last.profile, { silent: true })) {
      if (run === reconnectRun) {
        setReconnecting(null);
        toast("info", "Reconectado.");
      }
      return;
    }
    if (run !== reconnectRun) return;
    await new Promise((r) => setTimeout(r, RECONNECT_EVERY_MS));
  }
  if (run === reconnectRun) {
    setReconnecting(null);
    toast("error", `Não foi possível reconectar a ${last.target.label}. Confira se o outro computador está ligado e na rede.`);
  }
}

/** Para a reconexão automática (botão Cancelar ou nova conexão manual). */
export function cancelReconnect() {
  reconnectRun++;
  useStore.getState().setReconnecting(null);
}
