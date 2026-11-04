import { useEffect, useState } from "react";
import {
  api,
  errorMessage,
  platformLabel,
  PROFILE_LABEL,
  type DeviceRecord,
  type DiscoveredDevice,
  type Profile,
} from "../lib/api";
import { connectTo } from "../lib/session";
import { useStore } from "../store";

export function HomePage() {
  const local = useStore((s) => s.local);
  const discovered = useStore((s) => s.discovered);
  const knownVersion = useStore((s) => s.knownVersion);
  const toast = useStore((s) => s.toast);

  const [address, setAddress] = useState("");
  const [port, setPort] = useState("");
  const [profile, setProfile] = useState<Profile>("remote_control");
  const [password, setPassword] = useState("");
  const [remember, setRemember] = useState(true);
  const [known, setKnown] = useState<DeviceRecord[]>([]);

  const loadKnown = () => api.listKnownDevices().then(setKnown).catch(() => {});
  useEffect(() => {
    loadKnown();
  }, [knownVersion]);

  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    const p = port.trim() ? Number(port) : undefined;
    if (p !== undefined && (!Number.isInteger(p) || p < 1 || p > 65535)) {
      toast("error", "Porta inválida.");
      return;
    }
    connectTo(
      { address: address.trim(), port: p, label: address.trim(), password, rememberPassword: remember && !!password },
      profile,
    );
    setPassword("");
  };

  const connectDiscovered = (d: DiscoveredDevice) => {
    const addr = d.addresses[0];
    if (!addr) return toast("error", "Esse dispositivo não anunciou um endereço.");
    connectTo({ address: addr, port: d.port, expectedFingerprint: d.fingerprint, label: `${d.name} (${addr})` }, profile);
  };

  const connectKnown = (d: DeviceRecord) => {
    if (!d.lastIp) return toast("warning", "Endereço desconhecido; digite o IP.");
    connectTo(
      { address: d.lastIp, port: d.lastPort ?? undefined, label: `${d.displayName} (${d.lastIp})` },
      profile,
    );
  };

  const toggleFavorite = async (d: DeviceRecord) => {
    await api.setFavorite(d.id, !d.favorite).catch((e) => toast("error", errorMessage(e)));
    loadKnown();
  };

  const forget = async (d: DeviceRecord) => {
    await api.forgetDevice(d.id).catch((e) => toast("error", errorMessage(e)));
    toast("info", `${d.displayName} removido. A próxima conexão vai registrar a identidade de novo.`);
    loadKnown();
  };

  // Dispositivos conhecidos que estão online agora (pela descoberta).
  const onlineIds = new Set(discovered.map((d) => d.deviceId));

  return (
    <div className="home">
      <aside className="home__side">
        <LocalDeviceCard />
      </aside>

      <section className="home__main">
        <div className="card">
          <h2 className="card__title">Conectar a outro computador</h2>
          <form className="connect" onSubmit={submit}>
            <label className="field field--grow">
              <span className="field__label">IP ou nome do computador</span>
              <input
                value={address}
                onChange={(e) => setAddress(e.target.value)}
                placeholder="192.168.1.105"
                autoFocus
                spellCheck={false}
              />
            </label>
            <label className="field field--port">
              <span className="field__label">Porta</span>
              <input
                value={port}
                onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))}
                placeholder={String(local?.port ?? 47321)}
                inputMode="numeric"
              />
            </label>
            <label className="field">
              <span className="field__label">Pedir</span>
              <select value={profile} onChange={(e) => setProfile(e.target.value as Profile)}>
                <option value="remote_control">{PROFILE_LABEL.remote_control}</option>
                <option value="view_only">{PROFILE_LABEL.view_only}</option>
              </select>
            </label>
            <button className="btn btn--primary" type="submit" disabled={!address.trim()}>
              Conectar
            </button>
          </form>
          <div className="connect connect--secondary">
            <label className="field field--grow">
              <span className="field__label">Senha de acesso (opcional)</span>
              <input
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                placeholder="Sem senha: o outro computador precisa aceitar"
                autoComplete="off"
              />
            </label>
            <label className="check">
              <input type="checkbox" checked={remember} onChange={(e) => setRemember(e.target.checked)} />
              Lembrar senha
            </label>
          </div>
        </div>

        <div className="card">
          <div className="card__head">
            <h2 className="card__title">Na rede local</h2>
            <span className="muted small">
              {local?.discoveryActive ? "Procurando automaticamente…" : "Descoberta desativada"}
            </span>
          </div>
          {discovered.length === 0 ? (
            <p className="empty">
              Nenhum computador encontrado ainda. Abra o app no outro computador ou conecte pelo IP.
            </p>
          ) : (
            <ul className="devices">
              {discovered.map((d) => (
                <li key={d.deviceId} className="device">
                  <PlatformIcon platform={d.platform} />
                  <div className="device__info">
                    <div className="device__name">{d.name}</div>
                    <div className="device__meta">
                      {platformLabel(d.platform)} · {d.addresses[0] ?? "sem endereço"}
                      {d.port !== 47321 ? `:${d.port}` : ""}
                    </div>
                  </div>
                  <button className="btn btn--primary btn--small" onClick={() => connectDiscovered(d)}>
                    Conectar
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>

        <div className="card">
          <h2 className="card__title">Dispositivos conhecidos</h2>
          {known.length === 0 ? (
            <p className="empty">Computadores conectados antes aparecem aqui.</p>
          ) : (
            <ul className="devices">
              {known.map((d) => (
                <li key={d.id} className="device">
                  <button
                    className={d.favorite ? "star star--on" : "star"}
                    onClick={() => toggleFavorite(d)}
                    title={d.favorite ? "Remover dos favoritos" : "Favoritar"}
                    aria-label="Favorito"
                  >
                    ★
                  </button>
                  <div className="device__info">
                    <div className="device__name">
                      {d.displayName} {onlineIds.has(d.id) && <span className="pill pill--ok">online</span>}
                      {d.savedPassword && <span className="pill">🔑 senha salva</span>}
                    </div>
                    <div className="device__meta">
                      {platformLabel(d.osType)} · {d.lastIp ?? "IP desconhecido"}
                      {d.lastSeenAt ? ` · visto ${new Date(d.lastSeenAt).toLocaleString("pt-BR")}` : ""}
                    </div>
                    <div className="device__fp mono">{d.fingerprint}</div>
                  </div>
                  <div className="device__actions">
                    <button className="btn btn--small" onClick={() => connectKnown(d)} disabled={!d.lastIp}>
                      Conectar
                    </button>
                    {d.savedPassword && (
                      <button
                        className="btn btn--small btn--ghost"
                        onClick={async () => {
                          await api.forgetSavedPassword(d.id).catch((e) => toast("error", errorMessage(e)));
                          loadKnown();
                        }}
                        title="Apagar a senha lembrada"
                      >
                        Esquecer senha
                      </button>
                    )}
                    <button className="btn btn--small btn--ghost" onClick={() => forget(d)} title="Esquecer identidade">
                      Esquecer
                    </button>
                  </div>
                </li>
              ))}
            </ul>
          )}
        </div>
      </section>
    </div>
  );
}

function LocalDeviceCard() {
  const local = useStore((s) => s.local);
  const toast = useStore((s) => s.toast);
  if (!local) return <div className="card">Carregando…</div>;

  const copy = (text: string) => {
    navigator.clipboard?.writeText(text).then(
      () => toast("info", "Copiado."),
      () => {},
    );
  };

  return (
    <div className="card card--local">
      <div className="local__label">Este computador</div>
      <div className="local__name">{local.name}</div>
      <div className="muted small">
        {platformLabel(local.platform)} · {local.hostname}
      </div>

      <div className="local__section">
        <div className="field__label">Endereços para conexão</div>
        {local.interfaces.length === 0 && <div className="muted small">Nenhuma rede encontrada.</div>}
        {local.interfaces.map((i) => (
          <button key={i.name + i.ip} className="addr" onClick={() => copy(i.ip)} title="Copiar">
            <span className="mono">{i.ip}</span>
            <span className="muted small">{i.name}</span>
          </button>
        ))}
        <div className="muted small">Porta UDP {local.port}</div>
      </div>

      <div className="local__section">
        <div className="field__label">Fingerprint</div>
        <div className="mono small fp">{local.fingerprint}</div>
      </div>

      {local.listenError && <div className="alert alert--error">{local.listenError}</div>}

      {local.platform === "macos" && !local.screenPermission && (
        <div className="alert alert--warn">
          Para outros computadores verem esta tela, o macOS exige a permissão de <b>Gravação de Tela</b>.
          <button className="btn btn--small" onClick={() => api.requestScreenPermission()}>
            Conceder
          </button>
        </div>
      )}
      {local.platform === "macos" && !local.accessibilityPermission && (
        <div className="alert alert--warn">
          Para ser controlado remotamente, o macOS exige a permissão de <b>Acessibilidade</b>.
          <button className="btn btn--small" onClick={() => api.requestAccessibilityPermission()}>
            Abrir ajustes
          </button>
        </div>
      )}
    </div>
  );
}

function PlatformIcon({ platform }: { platform: string }) {
  return (
    <span className="platform" aria-hidden>
      {platform === "macos" ? "Mac" : platform === "windows" ? "Win" : "PC"}
    </span>
  );
}
