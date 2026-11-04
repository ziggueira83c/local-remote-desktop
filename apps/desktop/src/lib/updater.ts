// Atualização automática: o app consulta o latest.json da última versão publicada no GitHub,
// confere a assinatura (chave pública embutida no app) e instala por cima.
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

export type UpdateState =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "none" }
  | { kind: "available"; version: string; notes: string }
  | { kind: "downloading"; percent: number | null }
  | { kind: "ready" }
  | { kind: "error"; message: string };

type Listener = (s: UpdateState) => void;

let state: UpdateState = { kind: "idle" };
let pending: Update | null = null;
const listeners = new Set<Listener>();

function set(s: UpdateState) {
  state = s;
  for (const l of listeners) l(s);
}

export function subscribe(l: Listener): () => void {
  listeners.add(l);
  l(state);
  return () => listeners.delete(l);
}

export function currentState(): UpdateState {
  return state;
}

/** Procura uma versão nova. `quiet`: não mostra erro (verificação automática sem internet). */
export async function checkForUpdate(quiet = false): Promise<void> {
  if (state.kind === "downloading" || state.kind === "ready") return;
  set({ kind: "checking" });
  try {
    const u = await check({ timeout: 15_000 });
    pending = u;
    if (u) set({ kind: "available", version: u.version, notes: u.body ?? "" });
    else set({ kind: "none" });
  } catch (e) {
    set(quiet ? { kind: "idle" } : { kind: "error", message: String(e) });
  }
}

/** Baixa, instala e reinicia o app. */
export async function installUpdate(): Promise<void> {
  if (!pending) return;
  let total = 0;
  let got = 0;
  set({ kind: "downloading", percent: null });
  try {
    await pending.downloadAndInstall((ev) => {
      if (ev.event === "Started") total = ev.data.contentLength ?? 0;
      else if (ev.event === "Progress") {
        got += ev.data.chunkLength;
        set({ kind: "downloading", percent: total > 0 ? Math.min(100, (got / total) * 100) : null });
      } else if (ev.event === "Finished") set({ kind: "ready" });
    });
    set({ kind: "ready" });
    await relaunch();
  } catch (e) {
    set({ kind: "error", message: String(e) });
  }
}
