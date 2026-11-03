import { useEffect } from "react";
import { listen } from "@tauri-apps/api/event";
import { api, type NodeEvent } from "./lib/api";
import { useStore } from "./store";
import { Header } from "./components/Header";
import { InboundBanner } from "./components/InboundBanner";
import { IncomingRequestDialog } from "./components/IncomingRequestDialog";
import { ConnectingDialog } from "./components/ConnectingDialog";
import { Toasts } from "./components/Toasts";
import { HomePage } from "./pages/HomePage";
import { RemoteSessionPage } from "./pages/RemoteSessionPage";
import { SettingsPage } from "./pages/SettingsPage";
import { HistoryPage } from "./pages/HistoryPage";

export default function App() {
  const page = useStore((s) => s.page);
  const outbound = useStore((s) => s.outbound);

  useEffect(() => {
    const unlisten = listen<NodeEvent>("lrd-event", (e) => useStore.getState().handleEvent(e.payload));
    const refresh = () => {
      api.getLocalDevice().then(useStore.getState().setLocal).catch(() => {});
    };
    refresh();
    api.listDiscovered().then(useStore.getState().setDiscovered).catch(() => {});
    const t = setInterval(refresh, 5000);
    return () => {
      clearInterval(t);
      unlisten.then((f) => f());
    };
  }, []);

  const inSession = page === "session" && outbound;

  return (
    <div className={inSession ? "app app--session" : "app"}>
      {!inSession && <Header />}
      <InboundBanner />
      <main className="app__main">
        {page === "home" && <HomePage />}
        {page === "session" && (outbound ? <RemoteSessionPage /> : <HomePage />)}
        {page === "settings" && <SettingsPage />}
        {page === "history" && <HistoryPage />}
      </main>
      <IncomingRequestDialog />
      <ConnectingDialog />
      <Toasts />
    </div>
  );
}
