// Tipos e chamadas ao backend Rust (comandos Tauri).
import { Channel, invoke } from "@tauri-apps/api/core";

export type Platform = "windows" | "macos" | "other";
export type Profile = "view_only" | "remote_control" | "full_control";

export interface Permissions {
  viewScreen: boolean;
  controlMouse: boolean;
  controlKeyboard: boolean;
  syncClipboard: boolean;
  transferFiles: boolean;
  switchDisplays: boolean;
}

export const PROFILE_PERMISSIONS: Record<Profile, Permissions> = {
  view_only: {
    viewScreen: true,
    controlMouse: false,
    controlKeyboard: false,
    syncClipboard: false,
    transferFiles: false,
    switchDisplays: true,
  },
  remote_control: {
    viewScreen: true,
    controlMouse: true,
    controlKeyboard: true,
    syncClipboard: false,
    transferFiles: false,
    switchDisplays: true,
  },
  full_control: {
    viewScreen: true,
    controlMouse: true,
    controlKeyboard: true,
    syncClipboard: true,
    transferFiles: true,
    switchDisplays: true,
  },
};

export const PROFILE_LABEL: Record<Profile, string> = {
  view_only: "Somente visualizar",
  remote_control: "Controle remoto",
  full_control: "Controle completo",
};

export interface NetworkInterface {
  name: string;
  ip: string;
}

export interface LocalDeviceInfo {
  deviceId: string;
  name: string;
  hostname: string;
  platform: Platform;
  fingerprint: string;
  port: number;
  interfaces: NetworkInterface[];
  listening: boolean;
  listenError: string | null;
  discoveryActive: boolean;
  screenPermission: boolean;
  accessibilityPermission: boolean;
  inboundSession: string | null;
}

export interface DiscoveredDevice {
  deviceId: string;
  name: string;
  platform: Platform;
  fingerprint: string;
  addresses: string[];
  port: number;
  protocolVersion: number;
}

export interface DeviceRecord {
  id: string;
  hostname: string;
  displayName: string;
  osType: Platform;
  fingerprint: string;
  lastIp: string | null;
  lastPort: number | null;
  favorite: boolean;
  lastSeenAt: string | null;
  createdAt: string;
}

export interface SessionRecord {
  id: string;
  remoteDeviceId: string | null;
  remoteName: string | null;
  remoteAddress: string | null;
  direction: "inbound" | "outbound";
  status: string;
  permissions: string[];
  startedAt: string;
  endedAt: string | null;
  errorCode: string | null;
}

export interface RemoteDisplay {
  id: string;
  name: string;
  width: number;
  height: number;
  primary: boolean;
}

export interface AppSettings {
  deviceName: string;
  listenPort: number;
  discoveryEnabled: boolean;
  qualityProfile: "speed" | "balanced" | "quality";
  maxFps: number;
  swapCtrlCmd: boolean;
  requireApproval: boolean;
  shareAudio: boolean;
  receiveAudio: boolean;
  enhanceImage: boolean;
  enhanceStrength: number;
}

export type NodeEvent =
  | { type: "devicesChanged"; devices: DiscoveredDevice[] }
  | {
      type: "incomingRequest";
      sessionId: string;
      deviceId: string;
      deviceName: string;
      platform: Platform;
      address: string;
      fingerprint: string;
      sas: string;
      knownDevice: boolean;
      requestedProfile: Profile;
    }
  | { type: "incomingCancelled"; sessionId: string; reason: string }
  | { type: "inboundStarted"; sessionId: string; deviceName: string; address: string; permissions: Permissions }
  | { type: "inboundEnded"; sessionId: string; reason: string }
  | {
      type: "outboundState";
      state: string;
      remoteName: string | null;
      fingerprint: string | null;
      sas: string | null;
    }
  | {
      type: "outboundStarted";
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
  | { type: "outboundEnded"; sessionId: string; reason: string }
  | { type: "permissionsChanged"; sessionId: string; permissions: Permissions }
  | { type: "metrics"; sessionId: string; fps: number; kbps: number; rttMs: number }
  | { type: "notice"; level: "info" | "warning" | "error"; message: string };

export type InputEvent =
  | { type: "pointer"; displayId: string; x: number; y: number }
  | { type: "button"; button: number; pressed: boolean }
  | { type: "key"; hid: number; pressed: boolean }
  | { type: "scroll"; dx: number; dy: number };

export interface ConnectParams {
  address: string;
  port?: number;
  expectedFingerprint?: string;
  profile: Profile;
  codecs?: string[];
  audio?: boolean;
}

export const api = {
  getLocalDevice: () => invoke<LocalDeviceInfo>("get_local_device"),
  listDiscovered: () => invoke<DiscoveredDevice[]>("list_discovered"),
  listKnownDevices: () => invoke<DeviceRecord[]>("list_known_devices"),
  setFavorite: (deviceId: string, favorite: boolean) => invoke<void>("set_favorite", { deviceId, favorite }),
  forgetDevice: (deviceId: string) => invoke<void>("forget_device", { deviceId }),
  connect: (params: ConnectParams, onFrame: Channel<ArrayBuffer>) =>
    invoke<string>("connect_device", { params, onFrame }),
  disconnect: () => invoke<void>("disconnect_session"),
  sendInput: (event: InputEvent) => invoke<void>("send_input", { event }),
  selectDisplay: (displayId: string) => invoke<void>("select_display", { displayId }),
  setVideoQuality: (profile: string, maxFps: number) => invoke<void>("set_video_quality", { profile, maxFps }),
  requestKeyframe: () => invoke<void>("request_keyframe"),
  setAudio: (enabled: boolean) => invoke<void>("set_audio", { enabled }),
  approve: (sessionId: string, permissions: Permissions) => invoke<void>("approve_session", { sessionId, permissions }),
  reject: (sessionId: string) => invoke<void>("reject_session", { sessionId }),
  endInbound: () => invoke<void>("end_inbound_session"),
  setInboundPermissions: (permissions: Permissions) => invoke<Permissions>("set_inbound_permissions", { permissions }),
  getSettings: () => invoke<AppSettings>("get_settings"),
  saveSettings: (settings: AppSettings) => invoke<AppSettings>("save_settings", { settings }),
  listHistory: (limit = 200) => invoke<SessionRecord[]>("list_history", { limit }),
  clearHistory: () => invoke<void>("clear_history"),
  requestScreenPermission: () => invoke<boolean>("request_screen_permission"),
  requestAccessibilityPermission: () => invoke<void>("request_accessibility_permission"),
};

export function errorMessage(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return JSON.stringify(e);
}

export function platformLabel(p: string): string {
  return p === "windows" ? "Windows" : p === "macos" ? "macOS" : "Outro";
}
