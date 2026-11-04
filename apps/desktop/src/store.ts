// Estado visual da aplicação (Zustand). O Rust é a fonte autoritativa de conexão e permissões.
import { create } from "zustand";
import type {
  DiscoveredDevice,
  LocalDeviceInfo,
  NodeEvent,
  Permissions,
  Platform,
  Profile,
  RemoteDisplay,
} from "./lib/api";
import { endMedia } from "./lib/session";

export type Page = "home" | "session" | "settings" | "history";

export interface IncomingRequest {
  sessionId: string;
  deviceId: string;
  deviceName: string;
  platform: Platform;
  address: string;
  fingerprint: string;
  sas: string;
  knownDevice: boolean;
  requestedProfile: Profile;
  receivedAt: number;
}

export interface OutboundProgress {
  state: string; // connecting | authenticating | awaiting_approval | failed
  target: string;
  remoteName: string | null;
  fingerprint: string | null;
  sas: string | null;
}

export interface OutboundSession {
  sessionId: string;
  remoteName: string;
  remotePlatform: Platform;
  displays: RemoteDisplay[];
  activeDisplay: string;
  permissions: Permissions;
  fingerprint: string;
  sas: string;
  videoCodec: string;
  audioEnabled: boolean;
}

export interface InboundSession {
  sessionId: string;
  deviceName: string;
  address: string;
  permissions: Permissions;
}

export interface Toast {
  id: number;
  level: "info" | "warning" | "error";
  message: string;
}

export interface Metrics {
  fps: number;
  kbps: number;
  rttMs: number;
}

interface State {
  page: Page;
  local: LocalDeviceInfo | null;
  discovered: DiscoveredDevice[];
  incoming: IncomingRequest[];
  inbound: InboundSession | null;
  progress: OutboundProgress | null;
  outbound: OutboundSession | null;
  metrics: Metrics | null;
  toasts: Toast[];
  knownVersion: number;

  setPage: (p: Page) => void;
  setLocal: (l: LocalDeviceInfo) => void;
  setDiscovered: (d: DiscoveredDevice[]) => void;
  setProgress: (p: OutboundProgress | null) => void;
  setActiveDisplay: (id: string) => void;
  setInboundPermissions: (p: Permissions) => void;
  removeIncoming: (sessionId: string) => void;
  toast: (level: Toast["level"], message: string) => void;
  dismissToast: (id: number) => void;
  bumpKnown: () => void;
  handleEvent: (e: NodeEvent) => void;
}

let toastSeq = 1;

export const useStore = create<State>((set, get) => ({
  page: "home",
  local: null,
  discovered: [],
  incoming: [],
  inbound: null,
  progress: null,
  outbound: null,
  metrics: null,
  toasts: [],
  knownVersion: 0,

  setPage: (page) => set({ page }),
  setLocal: (local) => set({ local }),
  setDiscovered: (discovered) => set({ discovered }),
  setProgress: (progress) => set({ progress }),
  setActiveDisplay: (id) => {
    const o = get().outbound;
    if (o) set({ outbound: { ...o, activeDisplay: id } });
  },
  setInboundPermissions: (permissions) => {
    const i = get().inbound;
    if (i) set({ inbound: { ...i, permissions } });
  },
  removeIncoming: (sessionId) => set({ incoming: get().incoming.filter((r) => r.sessionId !== sessionId) }),
  toast: (level, message) => {
    const id = toastSeq++;
    set({ toasts: [...get().toasts, { id, level, message }] });
    setTimeout(() => get().dismissToast(id), level === "error" ? 9000 : 6000);
  },
  dismissToast: (id) => set({ toasts: get().toasts.filter((t) => t.id !== id) }),
  bumpKnown: () => set({ knownVersion: get().knownVersion + 1 }),

  handleEvent: (e) => {
    const s = get();
    switch (e.type) {
      case "devicesChanged":
        set({ discovered: e.devices });
        break;
      case "incomingRequest":
        set({
          incoming: [
            ...s.incoming.filter((r) => r.sessionId !== e.sessionId),
            {
              sessionId: e.sessionId,
              deviceId: e.deviceId,
              deviceName: e.deviceName,
              platform: e.platform,
              address: e.address,
              fingerprint: e.fingerprint,
              sas: e.sas,
              knownDevice: e.knownDevice,
              requestedProfile: e.requestedProfile,
              receivedAt: Date.now(),
            },
          ],
        });
        break;
      case "incomingCancelled":
        s.removeIncoming(e.sessionId);
        s.toast("info", e.reason === "approval_timeout" ? "Pedido de acesso expirou." : "O pedido de acesso foi cancelado.");
        break;
      case "inboundStarted":
        set({
          inbound: {
            sessionId: e.sessionId,
            deviceName: e.deviceName,
            address: e.address,
            permissions: e.permissions,
          },
        });
        s.bumpKnown();
        break;
      case "inboundEnded":
        if (s.inbound?.sessionId === e.sessionId) set({ inbound: null });
        s.toast("info", "A sessão de acesso a este computador terminou.");
        break;
      case "outboundState":
        if (s.progress) {
          set({
            progress: {
              ...s.progress,
              state: e.state,
              remoteName: e.remoteName ?? s.progress.remoteName,
              fingerprint: e.fingerprint ?? s.progress.fingerprint,
              sas: e.sas ?? s.progress.sas,
            },
          });
        }
        break;
      case "outboundStarted":
        set({
          progress: null,
          metrics: null,
          page: "session",
          outbound: {
            sessionId: e.sessionId,
            remoteName: e.remoteName,
            remotePlatform: e.remotePlatform,
            displays: e.displays,
            activeDisplay: e.activeDisplay,
            permissions: e.permissions,
            fingerprint: e.fingerprint,
            sas: e.sas,
            videoCodec: e.videoCodec,
            audioEnabled: e.audioEnabled,
          },
        });
        s.bumpKnown();
        break;
      case "outboundEnded":
        if (s.outbound?.sessionId === e.sessionId) {
          endMedia();
          set({ outbound: null, metrics: null, page: s.page === "session" ? "home" : s.page });
          s.toast("info", e.reason);
        }
        break;
      case "permissionsChanged":
        if (s.outbound?.sessionId === e.sessionId) {
          set({ outbound: { ...s.outbound, permissions: e.permissions } });
          s.toast(
            "info",
            e.permissions.controlMouse || e.permissions.controlKeyboard
              ? "O controle remoto foi liberado."
              : "O outro computador bloqueou o controle; somente visualização.",
          );
        }
        break;
      case "metrics":
        if (s.outbound?.sessionId === e.sessionId) set({ metrics: { fps: e.fps, kbps: e.kbps, rttMs: e.rttMs } });
        break;
      case "notice":
        s.toast(e.level, e.message);
        break;
    }
  },
}));
