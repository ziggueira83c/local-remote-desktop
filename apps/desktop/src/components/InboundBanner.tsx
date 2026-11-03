import { api, errorMessage } from "../lib/api";
import { useStore } from "../store";

/** Faixa sempre visível enquanto alguém acessa este computador (sessões nunca ficam ocultas). */
export function InboundBanner() {
  const inbound = useStore((s) => s.inbound);
  const setPerms = useStore((s) => s.setInboundPermissions);
  const toast = useStore((s) => s.toast);
  if (!inbound) return null;

  const controlling = inbound.permissions.controlMouse || inbound.permissions.controlKeyboard;

  const toggleControl = async () => {
    const next = {
      ...inbound.permissions,
      controlMouse: !controlling,
      controlKeyboard: !controlling,
    };
    try {
      setPerms(await api.setInboundPermissions(next));
    } catch (e) {
      toast("error", errorMessage(e));
    }
  };

  return (
    <div className="banner">
      <span className="banner__dot" />
      <span className="banner__text">
        <strong>{inbound.deviceName}</strong> ({inbound.address}) está{" "}
        {controlling ? "controlando" : "vendo"} a tela deste computador.
      </span>
      <button className="btn btn--small btn--ghost-light" onClick={toggleControl}>
        {controlling ? "Bloquear controle" : "Permitir controle"}
      </button>
      <button className="btn btn--small btn--danger" onClick={() => api.endInbound()}>
        Encerrar acesso
      </button>
    </div>
  );
}
