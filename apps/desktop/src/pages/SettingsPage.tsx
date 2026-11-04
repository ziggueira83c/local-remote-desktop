import { useEffect, useState } from "react";
import { api, errorMessage, type AppSettings } from "../lib/api";
import { useStore } from "../store";

export function SettingsPage() {
  const toast = useStore((s) => s.toast);
  const local = useStore((s) => s.local);
  const setLocal = useStore((s) => s.setLocal);
  const [s, setS] = useState<AppSettings | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    api.getSettings().then(setS).catch((e) => toast("error", errorMessage(e)));
  }, []);

  if (!s) return <div className="page">Carregando…</div>;

  const update = <K extends keyof AppSettings>(k: K, v: AppSettings[K]) => setS({ ...s, [k]: v });

  const save = async () => {
    setSaving(true);
    try {
      setS(await api.saveSettings(s));
      setLocal(await api.getLocalDevice());
      toast("info", "Configurações salvas.");
    } catch (e) {
      toast("error", errorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="page">
      <h1 className="page__title">Configurações</h1>

      <div className="card">
        <h2 className="card__title">Geral</h2>
        <label className="field">
          <span className="field__label">Nome deste computador</span>
          <input value={s.deviceName} onChange={(e) => update("deviceName", e.target.value)} maxLength={63} />
          <span className="field__help">É o nome que os outros computadores veem na lista e no pedido de acesso.</span>
        </label>
      </div>

      <div className="card">
        <h2 className="card__title">Rede</h2>
        <label className="field">
          <span className="field__label">Porta UDP</span>
          <input
            value={s.listenPort}
            onChange={(e) => update("listenPort", Number(e.target.value.replace(/\D/g, "")) || 0)}
            inputMode="numeric"
          />
          <span className="field__help">
            Padrão 47321. Libere essa porta UDP no firewall para redes privadas. Conexões vindas de fora da
            rede local são sempre recusadas.
          </span>
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={s.discoveryEnabled}
            onChange={(e) => update("discoveryEnabled", e.target.checked)}
          />
          Anunciar este computador e procurar outros na rede (mDNS)
        </label>
      </div>

      <div className="card">
        <h2 className="card__title">Vídeo (quando este computador é acessado)</h2>
        <div className="row">
          <label className="field">
            <span className="field__label">Qualidade</span>
            <select value={s.qualityProfile} onChange={(e) => update("qualityProfile", e.target.value as AppSettings["qualityProfile"])}>
              <option value="speed">Fluidez (até 1600 px, ~3 Mbps)</option>
              <option value="balanced">Equilibrado (até 1920 px, ~7 Mbps)</option>
              <option value="quality">Nitidez (até 2560 px, ~14 Mbps)</option>
            </select>
          </label>
          <label className="field">
            <span className="field__label">Quadros por segundo</span>
            <select value={s.maxFps} onChange={(e) => update("maxFps", Number(e.target.value))}>
              <option value={15}>15</option>
              <option value={30}>30</option>
              <option value={60}>60</option>
            </select>
          </label>
        </div>
        <span className="field__help">Quem controla pode mudar isso durante a sessão.</span>
        <label className="check">
          <input type="checkbox" checked={s.shareAudio} onChange={(e) => update("shareAudio", e.target.checked)} />
          Transmitir o áudio deste computador (o que toca nos alto-falantes)
        </label>
      </div>

      <div className="card">
        <h2 className="card__title">Ao controlar outro computador</h2>
        <label className="check">
          <input type="checkbox" checked={s.receiveAudio} onChange={(e) => update("receiveAudio", e.target.checked)} />
          Receber o áudio do outro computador
        </label>
        <label className="check">
          <input type="checkbox" checked={s.enhanceImage} onChange={(e) => update("enhanceImage", e.target.checked)} />
          Melhorar a imagem (nitidez adaptativa) por padrão
        </label>
        {s.enhanceImage && (
          <label className="field">
            <span className="field__label">Intensidade</span>
            <select value={s.enhanceStrength} onChange={(e) => update("enhanceStrength", Number(e.target.value))}>
              <option value={0.25}>Leve</option>
              <option value={0.5}>Média</option>
              <option value={0.85}>Forte</option>
            </select>
          </label>
        )}
        <span className="field__help">
          A melhoria é feita na sua tela, pela placa de vídeo: não aumenta o tráfego nem pesa no outro computador.
        </span>
      </div>

      <div className="card">
        <h2 className="card__title">Teclado</h2>
        <label className="check">
          <input type="checkbox" checked={s.swapCtrlCmd} onChange={(e) => update("swapCtrlCmd", e.target.checked)} />
          Entre Windows e Mac, trocar Ctrl ⇄ Cmd automaticamente (Ctrl+C no Windows vira Cmd+C no Mac)
        </label>
      </div>

      <div className="card">
        <h2 className="card__title">Segurança</h2>
        <ul className="bullets">
          <li>Toda conexão é criptografada (QUIC + TLS 1.3) e autenticada pelo certificado de cada computador.</li>
          <li>Todo acesso a este computador precisa ser aceito aqui, na hora. O acesso sem supervisão está desativado nesta versão.</li>
          <li>Você pode bloquear o controle ou encerrar o acesso a qualquer momento pela faixa no topo.</li>
        </ul>
        {local && (
          <div className="field">
            <span className="field__label">Fingerprint deste computador</span>
            <span className="mono small fp">{local.fingerprint}</span>
          </div>
        )}
      </div>

      <div className="page__actions">
        <button className="btn btn--primary" onClick={save} disabled={saving}>
          {saving ? "Salvando…" : "Salvar"}
        </button>
      </div>
    </div>
  );
}
