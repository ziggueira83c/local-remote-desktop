import { useEffect, useState } from "react";
import { checkForUpdate, installUpdate, subscribe, type UpdateState } from "../lib/updater";
import { useStore } from "../store";

const CHECK_EVERY_MS = 6 * 60 * 60 * 1000;

/** Avisa quando há versão nova. Nunca instala no meio de uma sessão. */
export function UpdateBanner() {
  const [st, setSt] = useState<UpdateState>({ kind: "idle" });
  const [dismissed, setDismissed] = useState<string | null>(null);
  const busy = useStore((s) => !!s.outbound || !!s.inbound);

  useEffect(() => subscribe(setSt), []);
  useEffect(() => {
    const first = setTimeout(() => checkForUpdate(true), 8000);
    const t = setInterval(() => checkForUpdate(true), CHECK_EVERY_MS);
    return () => {
      clearTimeout(first);
      clearInterval(t);
    };
  }, []);

  if (st.kind === "available" && dismissed !== st.version && !busy) {
    return (
      <div className="banner banner--update">
        <span className="banner__text">
          Nova versão disponível: <strong>{st.version}</strong>. A atualização leva menos de um minuto e o app
          reinicia sozinho.
        </span>
        <button className="btn btn--small btn--ghost-light" onClick={() => setDismissed(st.version)}>
          Depois
        </button>
        <button className="btn btn--small" onClick={() => installUpdate()}>
          Atualizar agora
        </button>
      </div>
    );
  }
  if (st.kind === "downloading" || st.kind === "ready") {
    return (
      <div className="banner banner--update">
        <span className="banner__text">
          {st.kind === "ready"
            ? "Instalando a atualização…"
            : `Baixando a atualização${st.percent != null ? ` (${st.percent.toFixed(0)}%)` : "…"}`}
        </span>
      </div>
    );
  }
  return null;
}
