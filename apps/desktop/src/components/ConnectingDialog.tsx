import { api } from "../lib/api";
import { cancelReconnect } from "../lib/session";
import { useStore } from "../store";

const LABEL: Record<string, string> = {
  connecting: "Conectando…",
  authenticating: "Verificando identidade…",
  awaiting_approval: "Aguardando aprovação no outro computador…",
  failed: "Falha na conexão",
};

export function ConnectingDialog() {
  const progress = useStore((s) => s.progress);
  const reconnecting = useStore((s) => s.reconnecting);
  if (!progress && !reconnecting) return null;

  const cancel = () => {
    cancelReconnect();
    api.disconnect().catch(() => {});
    useStore.getState().setProgress(null);
  };

  // Reconexão automática: entre as tentativas (ou tentando sem precisar de aprovação).
  if (reconnecting && (!progress || progress.state === "connecting" || progress.state === "failed")) {
    return (
      <div className="modal-backdrop">
        <div className="modal modal--narrow" role="dialog" aria-modal="true">
          <div className="spinner" aria-hidden />
          <h2 className="modal__title">Conexão perdida · reconectando…</h2>
          <p className="muted">
            {reconnecting.label} · tentativa {reconnecting.attempt}
          </p>
          <div className="modal__actions">
            <button className="btn btn--ghost" onClick={cancel}>
              Cancelar
            </button>
          </div>
        </div>
      </div>
    );
  }
  if (!progress) return null;

  return (
    <div className="modal-backdrop">
      <div className="modal modal--narrow" role="dialog" aria-modal="true">
        <div className="spinner" aria-hidden />
        <h2 className="modal__title">{LABEL[progress.state] ?? progress.state}</h2>
        <p className="muted">{progress.remoteName ? `${progress.remoteName} (${progress.target})` : progress.target}</p>

        {progress.sas && (
          <div className="sas">
            <div className="sas__label">Código de verificação</div>
            <div className="sas__code">{progress.sas}</div>
            <div className="sas__hint">
              O mesmo código deve aparecer no pedido de acesso do outro computador.
            </div>
          </div>
        )}
        {progress.fingerprint && (
          <p className="mono small muted" title="Fingerprint do outro computador">
            {progress.fingerprint}
          </p>
        )}

        <div className="modal__actions">
          <button className="btn btn--ghost" onClick={cancel}>
            Cancelar
          </button>
        </div>
      </div>
    </div>
  );
}
