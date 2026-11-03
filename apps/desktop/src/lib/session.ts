// Inicia conexões e encaminha os quadros de vídeo para o renderer da tela de sessão.
import { Channel } from "@tauri-apps/api/core";
import { api, errorMessage, type Profile } from "./api";
import type { FrameRenderer } from "./renderer";
import { useStore } from "../store";

let renderer: FrameRenderer | null = null;
let pending: unknown[] = [];

/** A tela de sessão registra seu renderer; quadros que chegaram antes são desenhados agora. */
export function attachRenderer(r: FrameRenderer) {
  renderer = r;
  const queued = pending;
  pending = [];
  for (const f of queued) r.push(f);
}

export function detachRenderer(r: FrameRenderer) {
  if (renderer === r) renderer = null;
}

export interface ConnectTarget {
  address: string;
  port?: number;
  expectedFingerprint?: string;
  label: string;
}

export async function connectTo(target: ConnectTarget, profile: Profile) {
  const { setProgress, toast, progress, outbound } = useStore.getState();
  if (progress || outbound) {
    toast("warning", "Já existe uma conexão em andamento.");
    return;
  }
  setProgress({ state: "connecting", target: target.label, remoteName: null, fingerprint: null, sas: null });
  pending = [];
  const channel = new Channel<ArrayBuffer>();
  channel.onmessage = (data) => {
    if (renderer) {
      renderer.push(data);
    } else {
      pending.push(data);
      if (pending.length > 400) pending = []; // a tela pedirá um quadro completo ao abrir
    }
  };
  try {
    await api.connect(
      {
        address: target.address,
        port: target.port,
        expectedFingerprint: target.expectedFingerprint,
        profile,
      },
      channel,
    );
  } catch (e) {
    useStore.getState().setProgress(null);
    const msg = errorMessage(e);
    if (!/cancel|closed_by_user|encerrada antes/i.test(msg)) toast("error", msg);
  }
}
