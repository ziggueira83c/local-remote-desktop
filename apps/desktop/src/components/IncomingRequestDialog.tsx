import { useEffect, useState } from "react";
import { api, errorMessage, platformLabel, PROFILE_LABEL, PROFILE_PERMISSIONS, type Profile } from "../lib/api";
import { useStore } from "../store";

const TIMEOUT_S = 60;

/** Pedido de acesso: mostra identidade, fingerprint, código de verificação e escopo. */
export function IncomingRequestDialog() {
  const req = useStore((s) => s.incoming[0]);
  const removeIncoming = useStore((s) => s.removeIncoming);
  const toast = useStore((s) => s.toast);
  const [profile, setProfile] = useState<Profile>("remote_control");
  const [now, setNow] = useState(Date.now());

  useEffect(() => {
    if (!req) return;
    // Por padrão concede o que foi pedido, limitado a "controle remoto".
    setProfile(req.requestedProfile === "view_only" ? "view_only" : "remote_control");
    const t = setInterval(() => setNow(Date.now()), 500);
    return () => clearInterval(t);
  }, [req?.sessionId]);

  if (!req) return null;
  const remaining = Math.max(0, TIMEOUT_S - Math.floor((now - req.receivedAt) / 1000));

  const approve = async () => {
    try {
      await api.approve(req.sessionId, PROFILE_PERMISSIONS[profile]);
    } catch (e) {
      toast("error", errorMessage(e));
    }
    removeIncoming(req.sessionId);
  };
  const reject = async () => {
    await api.reject(req.sessionId).catch(() => {});
    removeIncoming(req.sessionId);
  };

  const options: Profile[] =
    req.requestedProfile === "view_only" ? ["view_only"] : ["view_only", "remote_control"];

  return (
    <div className="modal-backdrop">
      <div className="modal" role="dialog" aria-modal="true" aria-labelledby="req-title">
        <div className="modal__eyebrow">Pedido de acesso · expira em {remaining}s</div>
        <h2 id="req-title" className="modal__title">
          {req.deviceName} quer acessar este computador
        </h2>

        <dl className="facts">
          <dt>Computador</dt>
          <dd>
            {req.deviceName} · {platformLabel(req.platform)} · {req.address}
          </dd>
          <dt>Dispositivo</dt>
          <dd>
            {req.knownDevice ? (
              <span className="pill pill--ok">Já conectou antes</span>
            ) : (
              <span className="pill pill--warn">Primeira conexão</span>
            )}
          </dd>
          <dt>Pediu</dt>
          <dd>{PROFILE_LABEL[req.requestedProfile]}</dd>
          <dt>Fingerprint</dt>
          <dd className="mono small">{req.fingerprint}</dd>
        </dl>

        <div className="sas">
          <div className="sas__label">Código de verificação</div>
          <div className="sas__code">{req.sas}</div>
          <div className="sas__hint">
            Confirme que o mesmo código aparece na tela de quem está pedindo. Se for diferente, recuse.
          </div>
        </div>

        <label className="field">
          <span className="field__label">Permitir</span>
          <select value={profile} onChange={(e) => setProfile(e.target.value as Profile)}>
            {options.map((p) => (
              <option key={p} value={p}>
                {PROFILE_LABEL[p]}
              </option>
            ))}
          </select>
        </label>

        <div className="modal__actions">
          <button className="btn btn--ghost" onClick={reject}>
            Recusar
          </button>
          <button className="btn btn--primary" onClick={approve} autoFocus>
            Aceitar
          </button>
        </div>
      </div>
    </div>
  );
}
