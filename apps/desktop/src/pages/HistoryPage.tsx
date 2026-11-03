import { useEffect, useState } from "react";
import { api, errorMessage, type SessionRecord } from "../lib/api";
import { useStore } from "../store";

const STATUS: Record<string, string> = {
  active: "Em andamento",
  closed: "Encerrada",
  failed: "Falhou",
  awaiting_approval: "Aguardando",
  connecting: "Conectando",
  authenticating: "Autenticando",
  reconnecting: "Reconectando",
};

function duration(a: string, b: string | null): string {
  if (!b) return "—";
  const s = Math.max(0, Math.round((new Date(b).getTime() - new Date(a).getTime()) / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}min ${s % 60}s`;
  return `${Math.floor(m / 60)}h ${m % 60}min`;
}

export function HistoryPage() {
  const toast = useStore((s) => s.toast);
  const [rows, setRows] = useState<SessionRecord[]>([]);

  const load = () => api.listHistory().then(setRows).catch((e) => toast("error", errorMessage(e)));
  useEffect(() => {
    load();
  }, []);

  const clear = async () => {
    await api.clearHistory().catch((e) => toast("error", errorMessage(e)));
    load();
  };

  return (
    <div className="page page--wide">
      <div className="page__head">
        <h1 className="page__title">Histórico de sessões</h1>
        <div>
          <button className="btn btn--small" onClick={load}>
            Atualizar
          </button>{" "}
          <button className="btn btn--small btn--ghost" onClick={clear} disabled={rows.length === 0}>
            Limpar
          </button>
        </div>
      </div>
      <div className="card card--flush">
        {rows.length === 0 ? (
          <p className="empty">Nenhuma sessão registrada.</p>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>Início</th>
                <th>Direção</th>
                <th>Computador</th>
                <th>Status</th>
                <th>Duração</th>
                <th>Detalhe</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={r.id}>
                  <td>{new Date(r.startedAt).toLocaleString("pt-BR")}</td>
                  <td>{r.direction === "inbound" ? "← Recebida" : "→ Feita"}</td>
                  <td>
                    {r.remoteName ?? "—"} <span className="muted small">{r.remoteAddress ?? ""}</span>
                  </td>
                  <td>{STATUS[r.status] ?? r.status}</td>
                  <td>{duration(r.startedAt, r.endedAt)}</td>
                  <td className="muted small">{r.errorCode ?? ""}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}
