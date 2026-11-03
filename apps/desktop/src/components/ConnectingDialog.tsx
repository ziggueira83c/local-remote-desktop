import { api } from "../lib/api";
import { useStore } from "../store";

const LABEL: Record<string, string> = {
  connecting: "Conectando…",
  authenticating: "Verificando identidade…",
  awaiting_approval: "Aguardando aprovação no outro computador…",
  failed: "Falha na conexão",
};

export function ConnectingDialog() {
  const progress = useStore((s) => s.progress);
  if (!progress) return null;

  const cancel = () => {
    api.disconnect().catch(() => {});
    useStore.getState().setProgress(null);
  };

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
