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

export async function connectTo(target: ConnectTarget, profile: Profile) {
  const { setProgress, toast, progress, outbound } = useStore.getState();
  if (progress || outbound) {
    toast("warning", "Já existe uma conexão em andamento.");
    return;
  }
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
  } catch (e) {
    useStore.getState().setProgress(null);
    if (engine === current) endMedia();
    const msg = errorMessage(e);
    if (!/cancel|closed_by_user|encerrada antes/i.test(msg)) toast("error", msg);
  }
}
