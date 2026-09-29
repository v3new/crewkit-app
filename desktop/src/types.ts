// Types mirroring crewkit-core's serialized reports.

// --- Types mirroring crewkit-core's serialized reports ---

export interface Kit {
  id: string;
  name: string;
  version: string | null;
  publisher: string;
  publisherKey: string | null;
  homepage: string | null;
  marketplaceName: string;
  channels: Record<string, string>;
  telemetry: { endpoint: string; notice: string | null } | null;
  bundles: { id: string; displayName: string | null; plugins: string[]; mcpServers: string[] }[];
  mcpServers: {
    id: string;
    url: string;
    displayName: string | null;
    transport: string | null;
    auth: string | null;
    docs: string | null;
    remove: boolean;
    description: string;
  }[];
  plugins: {
    name: string;
    zip: string | null;
    artifact: { url: string; sha256: string } | null;
    version: string | null;
    displayName: string | null;
    remove: boolean;
    description: string;
  }[];
}

export interface KitCard {
  kit: Kit;
  source: string;
  channel: string;
  bundle: string | null;
  newItems: ItemRef[];
  error: string | null;
  /// Published behind a login, and this machine has no live session.
  needsAuth: boolean;
}

export interface DetectedClient {
  id: string;
  name: string;
  appInstalled: boolean;
  appPath: string | null;
  appVersion: string | null;
  cliPath: string | null;
  cliVersion: string | null;
  files: { key: string; path: string; exists: boolean }[];
  restartRequired: boolean;
  notes: string | null;
  present: boolean;
}

export type ItemStatus = "installed" | "installed-foreign" | "not-installed" | "client-unavailable";

export interface ItemState {
  kind: "plugin" | "mcp";
  id: string;
  client: string;
  status: ItemStatus;
  path: string;
  note?: { kind: "foreign"; value: string } | { kind: "no-cowork-profile" };
  version: string | null;
  updatedAtMs: number | null;
}

export interface ScanReport {
  clients: DetectedClient[];
  items: ItemState[];
  auth: { id: string; authorized: boolean; renews: boolean }[];
}

export type StepStatus = "ok" | "skipped" | "failed";

export interface StepReport {
  step: string;
  client: string;
  status: StepStatus;
  message: string;
}

export interface InstallReport {
  steps: StepReport[];
  restartNeeded: string[];
  scan: ScanReport;
}

// --- Localization (English default; RU available) ---

export interface ItemRef {
  kind: string;
  id: string;
}

export interface KitPreview {
  id: string;
  name: string;
  publisher: string;
  channels: string[];
  bundles: { id: string; displayName: string | null }[];
}

export type Notification = { atUnix: number } & (
  | { kind: "kit-updated"; kit: string; items: string[] }
  | { kind: "new-items"; kit: string; items: ItemRef[] }
  | { kind: "restart-needed"; clients: string[] }
  | { kind: "app-updated"; version: string }
);

export interface BackgroundReport {
  steps: StepReport[];
  restartNeeded: string[];
}

export interface DeepLinkAdd {
  url: string;
  channel: string | null;
  bundle: string | null;
}
