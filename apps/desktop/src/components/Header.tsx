import { useStore, type Page } from "../store";

const TABS: { id: Page; label: string }[] = [
  { id: "home", label: "Conectar" },
  { id: "history", label: "Histórico" },
  { id: "settings", label: "Configurações" },
];

export function Header() {
  const page = useStore((s) => s.page);
  const setPage = useStore((s) => s.setPage);
  const outbound = useStore((s) => s.outbound);
  const local = useStore((s) => s.local);

  return (
    <header className="header">
      <div className="header__brand">
        <span className="header__logo" aria-hidden>
          <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2">
            <rect x="3" y="4" width="18" height="12" rx="2" />
            <path d="M9 20h6M12 16v4" />
          </svg>
        </span>
        <span className="header__title">Local Remote Desktop</span>
      </div>
      <nav className="header__nav">
        {TABS.map((t) => (
          <button
            key={t.id}
            className={page === t.id ? "tab tab--active" : "tab"}
            onClick={() => setPage(t.id)}
          >
            {t.label}
          </button>
        ))}
        {outbound && (
          <button className="tab tab--live" onClick={() => setPage("session")}>
            ● Sessão com {outbound.remoteName}
          </button>
        )}
      </nav>
      <div className={local?.listening ? "status status--ok" : "status status--bad"}>
        {local ? (local.listening ? "Pronto para receber conexões" : "Sem escuta na rede") : "Iniciando…"}
      </div>
    </header>
  );
}
