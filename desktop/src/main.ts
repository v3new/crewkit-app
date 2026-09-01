import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

/// Injected at build time from tauri.conf.json (see vite.config.ts).
declare const __APP_VERSION__: string;

// --- Types mirroring crewkit-core's serialized reports ---

interface Kit {
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

interface KitCard {
  kit: Kit;
  source: string;
  channel: string;
  bundle: string | null;
  error: string | null;
  /// Published behind a login, and this machine has no live session.
  needsAuth: boolean;
}

interface DetectedClient {
  id: string;
  name: string;
  appInstalled: boolean;
  appVersion: string | null;
  cliPath: string | null;
  cliVersion: string | null;
  files: { key: string; path: string; exists: boolean }[];
  restartRequired: boolean;
  notes: string | null;
  present: boolean;
}

type ItemStatus = "installed" | "installed-foreign" | "not-installed" | "client-unavailable";

interface ItemState {
  kind: "plugin" | "mcp";
  id: string;
  client: string;
  status: ItemStatus;
  detail: string;
  version: string | null;
  updatedAtMs: number | null;
}

interface ScanReport {
  clients: DetectedClient[];
  items: ItemState[];
  auth: { id: string; authorized: boolean }[];
}

type StepStatus = "ok" | "skipped" | "failed";

interface StepReport {
  step: string;
  client: string;
  status: StepStatus;
  message: string;
}

interface InstallReport {
  steps: StepReport[];
  restartNeeded: string[];
  scan: ScanReport;
}

// --- Localization (English default; RU available) ---

const STRINGS: Record<string, Record<string, string>> = {
  en: {
    rescan: "Rescan",
    install: "Install kit",
    installing: "Installing…",
    addKit: "Add",
    addKitToggle: "+ Add kit by URL",
    addKitPlaceholder: "https://…/kit.json",
    adding: "Verifying…",
    cancel: "Cancel",
    removeKit: "Remove kit",
    removeKitConfirm: "Remove kit?",
    everything: "Everything installed",
    ofInstalled: "installed",
    serversAuthorized: "authorized",
    noClients: "No supported clients found",
    installed: "Installed",
    adopt: "Take over",
    adoptHint: "Added outside CrewKit — click to take over management",
    notInstalled: "Not installed",
    authorized: "authorized",
    authorize: "Authorize",
    waitingBrowser: "Waiting for browser…",
    logout: "Log out",
    loggingOut: "Logging out…",
    remove: "Remove",
    removeConfirm: "Remove everywhere?",
    removing: "Removing…",
    details: "Details",
    copyLog: "Copy",
    copied: "Copied",
    restart: "Restart",
    restartTail: "to pick up the changes",
    scanning: "Scanning this computer…",
    updateAvailable: "CrewKit {v} is available",
    installUpdate: "Update & restart",
    updating: "Updating…",
    telemetryNote: "Reports installs to the publisher",
    telemetryWhat: "what is collected",
    channel: "Channel",
    bundle: "Bundle",
    allItems: "All items",
    signedBy: "signed · key pinned",
    builtin: "built into the app",
    found: "found",
    notFound: "not found",
    failedShort: "failed",
    retry: "Retry",
    kitUnavailable: "Kit unavailable",
    signIn: "Sign in",
    kitNeedsSignIn: "This kit is published for a signed-in audience — sign in to download it",
    emptyTitle: "No kits yet",
    emptyHint: "Paste a kit manifest URL from your publisher, or open a crewkit:// link.",
    mcpGroup: "MCP Servers",
    pluginGroup: "Plugins",
    installShort: "Install",
    removeQ: "Remove?",
    installAllTo: "Install all to {app}",
    removeAllFrom: "Remove all from {app}",
    confirmAgain: "Click again to confirm",
    selectedN: "{n} selected",
    installToLabel: "Install to",
    both: "Both",
    clearSel: "Clear",
    removeFrom: "Remove from {app}",
    installToApp: "Install to {app}",
    notSupported: "Not supported",
    notSupportedTip: "Transport `{t}` needs a newer CrewKit version.",
  },
  ru: {
    rescan: "Обновить",
    install: "Установить",
    installing: "Установка…",
    addKit: "Добавить",
    addKitToggle: "+ Добавить кит по URL",
    addKitPlaceholder: "https://…/kit.json",
    adding: "Проверка…",
    cancel: "Отмена",
    removeKit: "Убрать кит",
    removeKitConfirm: "Убрать кит?",
    everything: "Всё установлено",
    ofInstalled: "установлено",
    serversAuthorized: "авторизовано",
    noClients: "Клиенты не найдены",
    installed: "Установлено",
    adopt: "Взять на себя",
    adoptHint: "Добавлено вне CrewKit — нажмите, чтобы CrewKit взял управление на себя",
    notInstalled: "Не установлено",
    authorized: "авторизован",
    authorize: "Авторизовать",
    waitingBrowser: "Ждём браузер…",
    logout: "Выйти",
    loggingOut: "Выходим…",
    remove: "Удалить",
    removeConfirm: "Удалить везде?",
    removing: "Удаляем…",
    details: "Детали",
    copyLog: "Скопировать",
    copied: "Скопировано",
    restart: "Перезапустите",
    restartTail: "чтобы подхватить изменения",
    scanning: "Сканируем этот компьютер…",
    updateAvailable: "Доступен CrewKit {v}",
    installUpdate: "Обновить и перезапустить",
    updating: "Обновляем…",
    telemetryNote: "Сообщает издателю об установках",
    telemetryWhat: "что собирается",
    channel: "Канал",
    bundle: "Набор",
    allItems: "Все элементы",
    signedBy: "подписан · ключ закреплён",
    builtin: "встроен в приложение",
    found: "найден",
    notFound: "не найден",
    failedShort: "с ошибкой",
    retry: "Повторить",
    kitUnavailable: "Кит недоступен",
    signIn: "Войти",
    kitNeedsSignIn: "Кит закрыт авторизацией — войдите, чтобы скачать его",
    emptyTitle: "Пока нет китов",
    emptyHint: "Вставьте URL манифеста от вашего издателя или откройте crewkit://-ссылку.",
    mcpGroup: "MCP-серверы",
    pluginGroup: "Плагины",
    installShort: "Установить",
    removeQ: "Удалить?",
    installAllTo: "Установить всё в {app}",
    removeAllFrom: "Удалить всё из {app}",
    confirmAgain: "Нажмите ещё раз для подтверждения",
    selectedN: "выбрано: {n}",
    installToLabel: "Установить в",
    both: "Оба",
    clearSel: "Сбросить",
    removeFrom: "Удалить из {app}",
    installToApp: "Установить в {app}",
    notSupported: "Не поддерживается",
    notSupportedTip: "Транспорт `{t}` требует более новой версии CrewKit.",
  },
  es: {
    rescan: "Reescanear",
    install: "Instalar kit",
    installing: "Instalando…",
    addKit: "Añadir",
    addKitToggle: "+ Añadir kit por URL",
    addKitPlaceholder: "https://…/kit.json",
    adding: "Verificando…",
    cancel: "Cancelar",
    removeKit: "Quitar kit",
    removeKitConfirm: "¿Quitar kit?",
    everything: "Todo instalado",
    ofInstalled: "instalado",
    serversAuthorized: "autorizados",
    noClients: "No se encontraron clientes compatibles",
    installed: "Instalado",
    adopt: "Gestionar",
    adoptHint: "Añadido fuera de CrewKit — haz clic para que CrewKit lo gestione",
    notInstalled: "No instalado",
    authorized: "autorizado",
    authorize: "Autorizar",
    waitingBrowser: "Esperando al navegador…",
    logout: "Cerrar sesión",
    loggingOut: "Cerrando sesión…",
    remove: "Eliminar",
    removeConfirm: "¿Eliminar de todos?",
    removing: "Eliminando…",
    details: "Detalles",
    copyLog: "Copiar",
    copied: "Copiado",
    restart: "Reinicia",
    restartTail: "para aplicar los cambios",
    scanning: "Escaneando este equipo…",
    updateAvailable: "CrewKit {v} disponible",
    installUpdate: "Actualizar y reiniciar",
    updating: "Actualizando…",
    telemetryNote: "Informa de instalaciones al editor",
    telemetryWhat: "qué se recopila",
    channel: "Canal",
    bundle: "Paquete",
    allItems: "Todo",
    signedBy: "firmado · clave fijada",
    builtin: "integrado en la app",
    found: "encontrado",
    notFound: "no encontrado",
    failedShort: "con error",
    retry: "Reintentar",
    kitUnavailable: "Kit no disponible",
    signIn: "Iniciar sesión",
    kitNeedsSignIn: "Este kit es privado: inicia sesión para descargarlo",
    emptyTitle: "Aún no hay kits",
    emptyHint: "Pega la URL del manifiesto de tu editor o abre un enlace crewkit://.",
    mcpGroup: "Servidores MCP",
    pluginGroup: "Plugins",
    installShort: "Instalar",
    removeQ: "¿Quitar?",
    installAllTo: "Instalar todo en {app}",
    removeAllFrom: "Quitar todo de {app}",
    confirmAgain: "Haz clic de nuevo para confirmar",
    selectedN: "{n} seleccionados",
    installToLabel: "Instalar en",
    both: "Ambos",
    clearSel: "Limpiar",
    removeFrom: "Quitar de {app}",
    installToApp: "Instalar en {app}",
    notSupported: "No compatible",
    notSupportedTip: "El transporte `{t}` requiere una versión más reciente de CrewKit.",
  },
  zh: {
    rescan: "重新扫描",
    install: "安装套件",
    installing: "安装中…",
    addKit: "添加",
    addKitToggle: "+ 通过 URL 添加套件",
    addKitPlaceholder: "https://…/kit.json",
    adding: "验证中…",
    cancel: "取消",
    removeKit: "移除套件",
    removeKitConfirm: "移除套件？",
    everything: "全部已安装",
    ofInstalled: "已安装",
    serversAuthorized: "已授权",
    noClients: "未找到支持的客户端",
    installed: "已安装",
    adopt: "接管",
    adoptHint: "在 CrewKit 之外添加 — 点击由 CrewKit 接管",
    notInstalled: "未安装",
    authorized: "已授权",
    authorize: "授权",
    waitingBrowser: "等待浏览器…",
    logout: "退出登录",
    loggingOut: "正在退出…",
    remove: "移除",
    removeConfirm: "从所有客户端移除？",
    removing: "移除中…",
    details: "详情",
    copyLog: "复制",
    copied: "已复制",
    restart: "请重启",
    restartTail: "以应用更改",
    scanning: "正在扫描此电脑…",
    updateAvailable: "CrewKit {v} 已发布",
    installUpdate: "更新并重启",
    updating: "更新中…",
    telemetryNote: "向发布者报告安装情况",
    telemetryWhat: "收集内容",
    channel: "通道",
    bundle: "套装",
    allItems: "全部",
    signedBy: "已签名 · 密钥已固定",
    builtin: "内置于应用",
    found: "已找到",
    notFound: "未找到",
    failedShort: "失败",
    retry: "重试",
    kitUnavailable: "套件不可用",
    signIn: "登录",
    kitNeedsSignIn: "该套件需要登录后才能下载",
    emptyTitle: "还没有套件",
    emptyHint: "粘贴发布者提供的清单 URL，或打开 crewkit:// 链接。",
    mcpGroup: "MCP 服务器",
    pluginGroup: "插件",
    installShort: "安装",
    removeQ: "移除？",
    installAllTo: "全部安装到 {app}",
    removeAllFrom: "从 {app} 移除全部",
    confirmAgain: "再次点击以确认",
    selectedN: "已选 {n} 项",
    installToLabel: "安装到",
    both: "两者",
    clearSel: "清除",
    removeFrom: "从 {app} 移除",
    installToApp: "安装到 {app}",
    notSupported: "不支持",
    notSupportedTip: "传输协议 `{t}` 需要更新版本的 CrewKit。",
  },
};

let lang = localStorage.getItem("crewkit-lang") ?? "en";
const t = (key: string): string => STRINGS[lang]?.[key] ?? STRINGS.en[key] ?? key;

// --- Presentation of the two ecosystems ---

// Columns group each vendor's apps into one scope and are labeled by the
// vendor (Anthropic / OpenAI), never by an individual app or product brand.
// `surfaces` is what the status columns aggregate; `targets` is the set of
// adapter client ids a scoped install/remove for that column touches.
const COLUMNS = [
  {
    id: "anthropic",
    label: "Anthropic",
    surfaces: ["claude-code", "claude-desktop"],
    targets: ["claude-code", "claude-desktop"],
  },
  {
    id: "openai",
    label: "OpenAI",
    surfaces: ["codex"],
    targets: ["codex", "chatgpt-desktop"],
  },
];

type Column = (typeof COLUMNS)[number];

const SURFACE_LABEL: Record<string, string> = {
  "claude-code": "Claude Code",
  "claude-desktop": "Claude Desktop",
  codex: "Codex",
};

// --- Persistent event log ---

// The Details journal survives restarts: every recorded event (installs,
// removals, kit changes, app updates) is kept with its timestamp, newest
// last, in a backend file (events.json next to the kit registry), capped
// so it stays small.
type LogEntry = StepReport & { atMs: number };

interface EventLogFile {
  appVersion: string | null;
  entries: LogEntry[];
}

const LOG_MAX = 500;

/// Every save rewrites the whole file; chaining keeps saves ordered so
/// an older snapshot can never land after a newer one.
let logSaveChain = Promise.resolve();

function persistLog(): void {
  logSaveChain = logSaveChain.then(() =>
    invoke("save_event_log", { log: { appVersion: __APP_VERSION__, entries: logSteps } }).then(
      () => undefined,
      // A failed save only loses history, never breaks the app.
      () => undefined
    )
  );
}

function logEvents(...steps: StepReport[]): void {
  const atMs = Date.now();
  logSteps.push(...steps.map((s) => ({ ...s, atMs })));
  if (logSteps.length > LOG_MAX) logSteps.splice(0, logSteps.length - LOG_MAX);
  persistLog();
}

// --- State ---

// Platform hook for CSS: the toolbar is laid out around macOS traffic
// lights, while Windows keeps its native title bar above our header.
document.documentElement.dataset.platform = navigator.userAgent.includes("Windows")
  ? "windows"
  : "mac";

const app = document.querySelector<HTMLDivElement>("#app")!;
let kits: KitCard[] = [];
const scans = new Map<string, ScanReport>();
const installing = new Set<string>();
let currentStep = "";
let logSteps: LogEntry[] = [];
/// Only failures from this session light the red footer badge; older
/// ones are history. (The log may be front-trimmed, so compare by time.)
const sessionStartMs = Date.now();
/// Live install-step events appended during the current full install;
/// the final report's steps are authoritative and replace them.
let liveLogCount = 0;
let restartNeeded: string[] = [];
let fatalError: string | null = null;
let loaded = false;
const scanErrors = new Map<string, string>();
let appUpdate: string | null = null;
let updatingApp = false;
let appUpdateError = "";
let addKitOpen = false;
let addKitUrl = "";
let addingKit = false;
let addKitError = "";
let logOpen = false;
/// Transient "Copied" feedback on the log's copy button.
let logCopied = false;
const busy = new Set<string>();
const confirming = new Set<string>();
/// Multi-select for batch install/remove: keys are kit␟kind␟id.
const selected = new Set<string>();
/// Kit ids with a scoped (column/selection) operation in flight.
const working = new Set<string>();
/// The open column-header menu, as kit␟column, or null.
let openMenu: string | null = null;

/// Separator for composite data-arg keys; ids never contain a tab.
const SEP = "\t";

function esc(text: string): string {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}

function tip(content: string, body: string, cls = ""): string {
  return `<span class="tip ${cls}">${content}<span class="tipbox">${body}</span></span>`;
}

/// Paths and URLs have no spaces, so a tooltip left to itself breaks them
/// mid-word ("…/claude-\ncode/…/MacO\nS/claude"). A `<wbr>` after every
/// separator puts the breaks where the eye already expects them.
function escPath(text: string): string {
  return esc(text).replace(/[/\\]/g, "$&<wbr>");
}

/// One tooltip line: a label on the left, its verdict pinned right, so a
/// long name wraps under itself instead of stranding the verdict alone.
function tipRow(label: string, state: string, stateCls = ""): string {
  return `<div class="tip-row"><span class="tip-label">${label}</span><span class="tip-state ${stateCls}">${state}</span></div>`;
}

const LOCALES: Record<string, string> = { en: "en", ru: "ru", es: "es", zh: "zh-CN" };

function fmtDate(ms: number): string {
  return new Date(ms).toLocaleDateString(LOCALES[lang] ?? "en", {
    day: "numeric",
    month: "short",
    year: "numeric",
  });
}

function fmtTime(ms: number): string {
  return new Date(ms).toLocaleString(LOCALES[lang] ?? "en", {
    day: "numeric",
    month: "short",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

// --- Aggregation ---

function itemsFor(scan: ScanReport, kind: string, id: string, surfaces: string[]): ItemState[] {
  return scan.items.filter((i) => i.kind === kind && i.id === id && surfaces.includes(i.client));
}

function aggregate(states: ItemState[]): ItemStatus {
  const relevant = states.filter((s) => s.status !== "client-unavailable");
  if (relevant.length === 0) return "client-unavailable";
  if (relevant.some((s) => s.status === "installed-foreign")) return "installed-foreign";
  if (relevant.every((s) => s.status === "installed")) return "installed";
  return "not-installed";
}

/// One status cell = one control: "Install" installs the item into that
/// app's surfaces; "Installed" click-through-confirm removes it there.
function cellButton(card: KitCard, scan: ScanReport, kind: string, id: string, col: Column): string {
  const states = itemsFor(scan, kind, id, col.surfaces);
  const status = aggregate(states);
  const key = ["cell", card.kit.id, kind, id, col.id].join(SEP);
  const breakdown = states
    .map((s) => {
      const meta = [s.version ? `v${s.version}` : "", s.updatedAtMs ? fmtDate(s.updatedAtMs) : ""]
        .filter(Boolean)
        .join(" · ");
      return (
        tipRow(esc(SURFACE_LABEL[s.client] ?? s.client), esc(s.status.replace(/-/g, " "))) +
        `<div class="tip-sub">${meta ? `${esc(meta)} · ` : ""}${esc(s.detail)}</div>`
      );
    })
    .join("");
  let chip: string;
  let hint = "";
  if (busy.has(key)) {
    chip = `<span class="chip chip--busy"><span class="spinner spinner--chip"></span></span>`;
  } else if (status === "installed") {
    chip = confirming.has(key)
      ? `<button class="chip chip--danger cell-act" data-arg="${esc(key)}">${t("removeQ")}</button>`
      : `<button class="chip chip--ok cell-act" data-arg="${esc(key)}">${t("installed")}</button>`;
    hint = `<div class="tip-action">${esc(t("removeFrom").replace("{app}", col.label))}</div>`;
  } else if (status === "not-installed") {
    chip = `<button class="chip chip--install cell-act" data-arg="${esc(key)}">${t("installShort")}</button>`;
    hint = `<div class="tip-action">${esc(t("installToApp").replace("{app}", col.label))}</div>`;
  } else if (status === "installed-foreign") {
    chip = `<button class="chip chip--warn cell-act" data-arg="${esc(key)}">${t("adopt")}</button>`;
    hint = `<div class="tip-action">${esc(t("adoptHint"))}</div>`;
  } else {
    return `<span class="na">—</span>`;
  }
  const body = hint + breakdown;
  return body ? tip(chip, body) : chip;
}

function installedAnywhere(scan: ScanReport, kind: string, id: string): boolean {
  return scan.items.some((i) => i.kind === kind && i.id === id && i.status === "installed");
}

// --- Sections ---

/// Compact status group for the footer: one dot-pill per ecosystem
/// (surfaces and paths live in an upward tooltip) plus the global summary.
function renderFooterStatus(): string {
  const scan = scans.values().next().value as ScanReport | undefined;
  if (!scan) return "";
  const byId = (id: string) => scan.clients.find((c) => c.id === id);
  const cliOf = (id: string) => byId(id)?.cliPath ?? "";
  const versionOf = (id: string) => byId(id)?.cliVersion ?? "";
  const appVersionOf = (id: string) => byId(id)?.appVersion ?? "";
  const groups = [
    {
      label: "Anthropic",
      surfaces: [
        { name: "Claude Cowork / Desktop", found: byId("claude-desktop")?.appInstalled ?? false, path: "", version: appVersionOf("claude-desktop") },
        { name: "Claude Code CLI", found: !!cliOf("claude-code"), path: cliOf("claude-code"), version: versionOf("claude-code") },
      ],
    },
    {
      label: "OpenAI",
      surfaces: [
        { name: "ChatGPT / Codex app", found: byId("chatgpt-desktop")?.appInstalled ?? false, path: "", version: appVersionOf("chatgpt-desktop") },
        { name: "Codex CLI", found: !!cliOf("codex"), path: cliOf("codex"), version: versionOf("codex") },
      ],
    },
  ];
  const pills = groups
    .map((g) => {
      const found = g.surfaces.filter((s) => s.found);
      const detail = g.surfaces
        .map(
          (s) =>
            tipRow(
              `${esc(s.name)}${s.version ? `<span class="tip-dim"> v${esc(s.version)}</span>` : ""}`,
              s.found ? t("found") : t("notFound"),
              s.found ? "tip-state--on" : "tip-state--off"
            ) + (s.found && s.path ? `<div class="tip-sub tip-path">${escPath(s.path)}</div>` : "")
        )
        .join("");
      return tip(
        `<span class="client-inline"><span class="dot ${found.length ? "dot--on" : "dot--off"}"></span><b>${esc(g.label)}</b></span>`,
        detail,
        "tip--up"
      );
    })
    .join("");

  // Global summary across every kit.
  let total = 0;
  let installed = 0;
  let authTotal = 0;
  let authorized = 0;
  for (const scan of scans.values()) {
    const relevant = scan.items.filter((i) => i.status !== "client-unavailable");
    total += relevant.length;
    installed += relevant.filter((i) => i.status === "installed").length;
    authTotal += scan.auth.length;
    authorized += scan.auth.filter((a) => a.authorized).length;
  }
  const allDone = total > 0 && installed === total;
  const summary =
    total === 0
      ? t("noClients")
      : `${allDone ? t("everything") : `${installed}/${total} ${t("ofInstalled")}`} · ${authorized}/${authTotal} ${t("serversAuthorized")}`;

  // Failures must be visible without opening the log: a per-cell install
  // whose only steps failed otherwise looks like "nothing happened".
  const failedCount = logSteps.filter((s) => s.status === "failed" && s.atMs >= sessionStartMs).length;
  return `<div class="foot-status">
    ${pills}
    <span class="foot-summary ${allDone ? "status-ok" : ""}">${esc(summary)}</span>
    ${failedCount ? `<span class="foot-summary status-fail">${failedCount} ${t("failedShort")}</span>` : ""}
  </div>`;
}

function actionLink(cls: string, data: string, label: string, disabled = false): string {
  return `<button class="link ${cls}" data-arg="${esc(data)}" ${disabled ? "disabled" : ""}>${label}</button>`;
}

function removeAction(kitId: string, scan: ScanReport, kind: string, id: string): string {
  if (!installedAnywhere(scan, kind, id)) return "";
  const key = `${kitId} ${kind} ${id}`;
  if (busy.has(key)) return `<span class="muted-action">${t("removing")}</span>`;
  return confirming.has(key)
    ? actionLink("remove confirm", key, t("removeConfirm"))
    : actionLink("remove hover-action", key, t("remove"));
}

/// Every selectable item of a kit, MCP servers first (they lead the list).
function kitItems(card: KitCard): { kind: string; id: string }[] {
  return [
    ...card.kit.mcpServers.filter((s) => !s.remove).map((s) => ({ kind: "mcp", id: s.id })),
    ...card.kit.plugins
      .filter((p) => !p.remove)
      .map((p) => ({ kind: "plugin", id: `${p.name}@${card.kit.marketplaceName}` })),
  ];
}

function itemKey(kitId: string, kind: string, id: string): string {
  return [kitId, kind, id].join(SEP);
}

function selCell(kitId: string, kind: string, id: string): string {
  const key = itemKey(kitId, kind, id);
  return `<span class="sel-cell"><input type="checkbox" class="sel-row" data-arg="${esc(key)}" ${selected.has(key) ? "checked" : ""}></span>`;
}

/// Column header as a pull-down: install or remove everything for one app.
function colHead(card: KitCard, col: Column): string {
  const menuKey = `${card.kit.id}${SEP}${col.id}`;
  const open = openMenu === menuKey;
  const removeKey = `eco-remove${SEP}${menuKey}`;
  const menu = open
    ? `<div class="col-menu">
        <button class="menu-item eco-install" data-arg="${esc(menuKey)}">${esc(t("installAllTo").replace("{app}", col.label))}</button>
        <button class="menu-item menu-item--danger eco-remove" data-arg="${esc(menuKey)}">${
          confirming.has(removeKey) ? esc(t("confirmAgain")) : esc(t("removeAllFrom").replace("{app}", col.label))
        }</button>
      </div>`
    : "";
  return `<span class="col-wrap"><button class="col-head${open ? " col-head--open" : ""}" data-arg="${esc(menuKey)}">${esc(col.label)}<span class="caret">▾</span></button>${menu}</span>`;
}

function renderRows(card: KitCard, scan: ScanReport): string {
  const kit = card.kit;
  const allKeys = kitItems(card).map((i) => itemKey(kit.id, i.kind, i.id));
  const allSelected = allKeys.length > 0 && allKeys.every((k) => selected.has(k));
  const head = `<div class="grid-head">
    <span class="sel-cell"><input type="checkbox" class="sel-all" data-arg="${esc(kit.id)}" ${allSelected ? "checked" : ""}></span>
    <span></span>
    ${COLUMNS.map((c) => `<span class="col-cell">${colHead(card, c)}</span>`).join("")}
  </div>`;

  const mcpRows = kit.mcpServers
    .filter((s) => !s.remove)
    .map((s) => {
      // Spec 1.0 defines only Streamable HTTP; anything else is listed
      // but not installable by this version.
      const unsupported = !!s.transport && s.transport !== "http";
      const cells = COLUMNS.map((c) => {
        const cell = unsupported
          ? tip(`<span class="chip chip--off">${t("notSupported")}</span>`, `<div>${esc(t("notSupportedTip").replace("{t}", s.transport!))}</div>`)
          : cellButton(card, scan, "mcp", s.id, c);
        return `<div class="col-cell">${cell}</div>`;
      }).join("");
      // A server absent from the auth report is an open endpoint
      // ("auth": "none") — there is no session to manage.
      const authState = scan.auth.find((a) => a.id === s.id);
      const isBusy = busy.has(`auth:${s.id}`);
      const auth = !authState
        ? ""
        : authState.authorized
          ? `<span class="chip chip--ok">${t("authorized")}</span>
             ${actionLink("deauthorize hover-action", s.id, isBusy ? t("loggingOut") : t("logout"), isBusy)}`
          : installedAnywhere(scan, "mcp", s.id)
            ? actionLink("authorize", s.id, isBusy ? t("waitingBrowser") : t("authorize"), isBusy)
            : "";
      const tipBody = `<div>${esc(s.id)}</div><div class="tip-sub tip-path">${escPath(s.url)}</div>${
        s.docs ? `<div class="tip-sub tip-path"><a href="${esc(s.docs)}" target="_blank">${escPath(s.docs)}</a></div>` : ""
      }`;
      return `<div class="row">
        ${selCell(kit.id, "mcp", s.id)}
        <div>
          <div class="item-name">${tip(esc(s.displayName ?? s.id), tipBody)}
            <span class="item-actions">${auth}${removeAction(kit.id, scan, "mcp", s.id)}</span>
          </div>
          ${s.description ? `<div class="item-sub">${esc(s.description)}</div>` : ""}
        </div>${cells}</div>`;
    })
    .join("");

  const pluginRows = kit.plugins
    .filter((p) => !p.remove)
    .map((p) => {
      const id = `${p.name}@${kit.marketplaceName}`;
      const cells = COLUMNS.map((c) => `<div class="col-cell">${cellButton(card, scan, "plugin", id, c)}</div>`).join("");
      const installs = scan.items.filter((i) => i.kind === "plugin" && i.id === id && i.version);
      const version = installs[0]?.version ?? p.version;
      const updated = installs.map((i) => i.updatedAtMs ?? 0).reduce((a, b) => Math.max(a, b), 0);
      const meta = [version ? `v${version}` : "", updated ? fmtDate(updated) : ""].filter(Boolean).join(" · ");
      return `<div class="row">
        ${selCell(kit.id, "plugin", id)}
        <div>
          <div class="item-name">${tip(esc(p.displayName ?? p.name), `<div>${esc(id)}</div>${meta ? `<div class="tip-sub">${esc(meta)}</div>` : ""}`)}
            <span class="item-actions">${removeAction(kit.id, scan, "plugin", id)}</span>
          </div>
          <div class="item-sub">${esc(p.description)}</div>
        </div>${cells}</div>`;
    })
    .join("");

  // MCP servers lead: they are what a kit's clients talk to first.
  const mcpSection = mcpRows ? `<div class="group-label">${t("mcpGroup")}</div>${mcpRows}` : "";
  const pluginSection = pluginRows ? `<div class="group-label">${t("pluginGroup")}</div>${pluginRows}` : "";
  return head + mcpSection + pluginSection;
}

/// Sticky batch-action bar shown while any rows are checkbox-selected.
function renderSelectionBar(): string {
  if (selected.size === 0) return "";
  const disabled = working.size > 0 ? "disabled" : "";
  return `<div class="selbar">
    <span class="selbar-count">${esc(t("selectedN").replace("{n}", String(selected.size)))}</span>
    <span class="selbar-label">${t("installToLabel")}</span>
    <button class="tb-btn sel-install" data-arg="anthropic" ${disabled}>Anthropic</button>
    <button class="tb-btn sel-install" data-arg="openai" ${disabled}>OpenAI</button>
    <button class="tb-btn sel-install" data-arg="both" ${disabled}>${t("both")}</button>
    <span class="spacer"></span>
    <button class="link remove sel-remove ${confirming.has("sel-remove") ? "confirm" : ""}" ${disabled}>
      ${confirming.has("sel-remove") ? t("removeConfirm") : t("remove")}
    </button>
    <button class="link sel-clear">${t("clearSel")}</button>
  </div>`;
}

function renderKitSection(card: KitCard): string {
  const kit = card.kit;
  const cardError = card.error ?? scanErrors.get(kit.id) ?? null;
  // Both dead-end cards below offer the same escape hatch.
  const dropKit = confirming.has(`kit ${kit.id}`)
    ? actionLink("remove-kit confirm", kit.id, t("removeKitConfirm"))
    : actionLink("remove-kit", kit.id, t("removeKit"));
  // A kit waiting for a login is not broken: it gets a sign-in button,
  // not the red box a failed fetch gets.
  if (card.needsAuth) {
    const waiting = busy.has(`auth:${kit.id}`);
    return `<section class="kit">
      <div class="kit-head">
        <span class="kit-name-big">${esc(kit.name)}</span>
        <span class="kit-meta">${esc(card.source)}</span>
        <span class="spacer"></span>
        ${dropKit}
        <button class="tb-btn primary kit-authorize" data-arg="${esc(kit.id)}" ${waiting ? "disabled" : ""}>${waiting ? t("waitingBrowser") : t("signIn")}</button>
      </div>
      <div class="kit-note">${t("kitNeedsSignIn")}</div>
    </section>`;
  }
  if (cardError) {
    return `<section class="kit">
      <div class="kit-head">
        <span class="kit-name-big">${esc(kit.name)}</span>
        <span class="kit-meta">${esc(card.source)}</span>
        <span class="spacer"></span>
        ${dropKit}
        <button class="tb-btn primary retry-kit">${t("retry")}</button>
      </div>
      <div class="kit-error">${t("kitUnavailable")} — ${esc(cardError)}</div>
    </section>`;
  }
  const scan = scans.get(kit.id);
  const sourceInfo =
    card.source === "builtin"
      ? `<div>${t("builtin")}</div>`
      : `<div>${esc(card.source)}</div><div class="tip-sub">${t("signedBy")}</div>`;
  const channels = Object.keys(kit.channels);
  const channelSelect = channels.length
    ? `<label class="sel">${t("channel")}
        <select class="channel" data-arg="${esc(kit.id)}">
          ${channels.map((c) => `<option value="${esc(c)}" ${c === card.channel ? "selected" : ""}>${esc(c)}</option>`).join("")}
        </select></label>`
    : "";
  const bundleSelect = kit.bundles.length
    ? `<label class="sel">${t("bundle")}
        <select class="bundle" data-arg="${esc(kit.id)}">
          <option value="">${t("allItems")}</option>
          ${kit.bundles.map((b) => `<option value="${esc(b.id)}" ${b.id === card.bundle ? "selected" : ""}>${esc(b.displayName ?? b.id)}</option>`).join("")}
        </select></label>`
    : "";
  const removeKitKey = `kit ${kit.id}`;
  const removeKit =
    card.source === "builtin"
      ? ""
      : confirming.has(removeKitKey)
        ? actionLink("remove-kit confirm", kit.id, t("removeKitConfirm"))
        : actionLink("remove-kit", kit.id, t("removeKit"));
  const telemetry = kit.telemetry
    ? `<div class="telemetry">${t("telemetryNote")}${
        kit.telemetry.notice ? ` · <a href="${esc(kit.telemetry.notice)}" target="_blank">${t("telemetryWhat")}</a>` : ""
      }</div>`
    : "";
  const kitBusy = installing.has(kit.id) || working.has(kit.id);
  const progress = kitBusy
    ? `<div class="progress"><span class="spinner"></span>${esc(currentStep || t("installing"))}</div>`
    : "";

  return `<section class="kit">
    <div class="kit-head">
      ${tip(`<span class="kit-name-big">${esc(kit.name)}</span>`, sourceInfo)}
      <span class="kit-meta">${esc(kit.publisher)}${kit.version ? ` · v${esc(kit.version)}` : ""}</span>
      ${channelSelect}${bundleSelect}
      <span class="spacer"></span>
      ${removeKit}
      <button class="tb-btn primary install" data-arg="${esc(kit.id)}" ${kitBusy ? "disabled" : ""}>
        ${installing.has(kit.id) ? t("installing") : t("install")}
      </button>
    </div>
    ${telemetry}${progress}
    ${scan ? renderRows(card, scan) : `<div class="loading">${t("scanning")}</div>`}
  </section>`;
}

function renderAddKit(): string {
  if (!addKitOpen) {
    return `<button id="add-kit-toggle" class="add-kit-toggle">${t("addKitToggle")}</button>`;
  }
  return `<div class="add-kit">
    <input id="add-kit-url" type="text" placeholder="${t("addKitPlaceholder")}" value="${esc(addKitUrl)}" ${addingKit ? "disabled" : ""} />
    <button id="add-kit-btn" class="tb-btn" ${addingKit || !addKitUrl.trim() ? "disabled" : ""}>${addingKit ? t("adding") : t("addKit")}</button>
    <button id="add-kit-cancel" class="link">${t("cancel")}</button>
    ${addKitError ? `<div class="add-kit-error">${esc(addKitError)}</div>` : ""}
  </div>`;
}

function renderLog(): string {
  if (logSteps.length === 0) return `<span class="log"></span>`;
  return `<span class="log">
    <button id="log-toggle" class="link log-toggle${logOpen ? " open" : ""}">${t("details")} (${logSteps.length})</button>
    ${logOpen ? `<button id="log-copy" class="link">${logCopied ? t("copied") : t("copyLog")}</button>` : ""}
  </span>`;
}

/// The journal renders as flex spans, so selecting it by hand copies the
/// columns glued together — this builds the plain-text form instead,
/// prefixed with the detected clients so a pasted log carries the
/// context support always asks for (which CLI, which version, where).
function logAsText(): string {
  const clients = new Map<string, DetectedClient>();
  for (const scan of scans.values()) for (const c of scan.clients) clients.set(c.id, c);
  const header = [`CrewKit v${__APP_VERSION__}`];
  for (const c of clients.values()) {
    const parts: string[] = [];
    if (c.appInstalled) parts.push(`app${c.appVersion ? ` v${c.appVersion}` : ""}`);
    if (c.cliPath) parts.push(`${c.cliVersion ? `v${c.cliVersion} ` : ""}${c.cliPath}`);
    header.push(`# ${c.id}: ${parts.length ? parts.join(" · ") : c.present ? "present (no CLI)" : "not found"}`);
  }
  const lines = logSteps.map(
    (s) => `${fmtTime(s.atMs)}\t${s.status}\t${s.client}\t${s.step}\t${s.message}`
  );
  return [...header, ...lines].join("\n");
}

async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // The async clipboard can be unavailable on the webview's custom
    // scheme; fall back to a selection-based copy.
    const ta = document.createElement("textarea");
    ta.value = text;
    document.body.appendChild(ta);
    ta.select();
    document.execCommand("copy");
    ta.remove();
  }
}

function renderLogRows(): string {
  if (!logOpen || logSteps.length === 0) return "";
  const rows = logSteps
    .map(
      (s) => `
      <div class="log-row log-row--${s.status}">
        <span class="log-time">${esc(fmtTime(s.atMs))}</span>
        <span class="log-status">${s.status}</span>
        <span class="log-client">${esc(s.client)}</span>
        <span class="log-step">${esc(s.step)}</span>
        <span class="log-message">${esc(s.message)}</span>
      </div>`
    )
    .join("");
  return `<div class="log-rows">${rows}</div>`;
}

function render(): void {
  if (fatalError) {
    app.innerHTML = `
      <header class="toolbar" data-tauri-drag-region>
        <span class="wordmark">CrewKit</span>
        <div class="tb-spacer" data-tauri-drag-region></div>
        <button id="retry-all" class="tb-btn primary">${t("retry")}</button>
      </header>
      <main><div class="fatal">${esc(fatalError)}</div></main>
      <footer></footer>`;
    document.querySelector("#retry-all")?.addEventListener("click", () => {
      fatalError = null;
      render();
      void rescanAll();
    });
    return;
  }
  if (!loaded) {
    app.innerHTML = `
      <header class="toolbar" data-tauri-drag-region><span class="wordmark">CrewKit</span></header>
      <div class="loading">${t("scanning")}</div>
      <footer></footer>`;
    return;
  }
  if (kits.length === 0) {
    addKitOpen = true;
  }
  app.innerHTML = `
    <header class="toolbar" data-tauri-drag-region>
      <span class="wordmark">CrewKit</span>
      <div class="tb-spacer" data-tauri-drag-region></div>
      <button id="rescan" class="tb-btn" ${installing.size || working.size ? "disabled" : ""}>${t("rescan")}</button>
    </header>
    <main class="${selected.size ? "selecting" : ""}">
      ${appUpdate ? `<div class="banner">${esc(t("updateAvailable").replace("{v}", appUpdate))}<button id="install-update" class="banner-btn" ${updatingApp ? "disabled" : ""}>${updatingApp ? t("updating") : t("installUpdate")}</button>${appUpdateError ? ` ${esc(appUpdateError)}` : ""}</div>` : ""}
      ${restartNeeded.length ? `<div class="banner">${t("restart")} ${restartNeeded.map(esc).join(", ")} ${t("restartTail")}</div>` : ""}
      ${kits.length === 0 ? `<div class="empty"><div class="empty-title">${t("emptyTitle")}</div><div class="empty-hint">${t("emptyHint")}</div></div>` : ""}
      ${kits.map(renderKitSection).join("")}
      ${renderAddKit()}
      ${renderSelectionBar()}
    </main>
    <footer>
      ${renderFooterStatus()}
      ${renderLog()}
      <span class="foot-side">
        <select id="lang-select" class="sel-lang">
          ${[
            ["en", "🇺🇸 English"],
            ["ru", "🇷🇺 Русский"],
            ["es", "🇪🇸 Español"],
            ["zh", "🇨🇳 中文"],
          ]
            .map(([code, label]) => `<option value="${code}" ${code === lang ? "selected" : ""}>${label}</option>`)
            .join("")}
        </select>
        <span class="version">v${__APP_VERSION__}</span>
      </span>
      ${renderLogRows()}
    </footer>`;

  document.querySelector("#rescan")?.addEventListener("click", () => void rescanAll());
  document.querySelector("#install-update")?.addEventListener("click", () => void installAppUpdate());
  document.querySelector("#log-toggle")?.addEventListener("click", () => {
    logOpen = !logOpen;
    render();
  });
  document.querySelector("#log-copy")?.addEventListener("click", () => {
    void copyText(logAsText()).then(() => {
      logCopied = true;
      render();
      setTimeout(() => {
        logCopied = false;
        render();
      }, 1500);
    });
  });
  // Each render rebuilds the footer, so re-pin the open log to its
  // newest entry.
  const logRows = document.querySelector(".log-rows");
  if (logRows) logRows.scrollTop = logRows.scrollHeight;
  app.querySelectorAll<HTMLButtonElement>("button.install").forEach((b) =>
    b.addEventListener("click", () => void install(b.dataset.arg!))
  );
  app.querySelectorAll<HTMLButtonElement>("button.authorize").forEach((b) =>
    b.addEventListener("click", () => void authAction("authorize", b.dataset.arg!))
  );
  app.querySelectorAll<HTMLButtonElement>("button.deauthorize").forEach((b) =>
    b.addEventListener("click", () => void authAction("deauthorize", b.dataset.arg!))
  );
  app.querySelectorAll<HTMLButtonElement>("button.remove").forEach((b) =>
    b.addEventListener("click", () => void removeItem(b.dataset.arg!))
  );
  app.querySelectorAll<HTMLButtonElement>("button.remove-kit").forEach((b) =>
    b.addEventListener("click", () => void removeKit(b.dataset.arg!))
  );
  app.querySelectorAll<HTMLButtonElement>("button.cell-act").forEach((b) =>
    b.addEventListener("click", () => void cellAction(b.dataset.arg!))
  );
  app.querySelectorAll<HTMLButtonElement>("button.col-head").forEach((b) =>
    b.addEventListener("click", () => {
      openMenu = openMenu === b.dataset.arg ? null : b.dataset.arg!;
      render();
    })
  );
  app.querySelectorAll<HTMLButtonElement>("button.eco-install").forEach((b) =>
    b.addEventListener("click", () => {
      openMenu = null;
      void ecoInstall(b.dataset.arg!);
    })
  );
  app.querySelectorAll<HTMLButtonElement>("button.eco-remove").forEach((b) =>
    b.addEventListener("click", () => void ecoRemove(b.dataset.arg!))
  );
  app.querySelectorAll<HTMLInputElement>("input.sel-row").forEach((cb) =>
    cb.addEventListener("change", () => {
      if (cb.checked) selected.add(cb.dataset.arg!);
      else selected.delete(cb.dataset.arg!);
      confirming.delete("sel-remove");
      render();
    })
  );
  app.querySelectorAll<HTMLInputElement>("input.sel-all").forEach((cb) => {
    const card = kits.find((k) => k.kit.id === cb.dataset.arg);
    if (!card) return;
    const keys = kitItems(card).map((i) => itemKey(card.kit.id, i.kind, i.id));
    const chosen = keys.filter((k) => selected.has(k)).length;
    cb.indeterminate = chosen > 0 && chosen < keys.length;
    cb.addEventListener("change", () => {
      const all = keys.every((k) => selected.has(k));
      for (const k of keys) {
        if (all) selected.delete(k);
        else selected.add(k);
      }
      confirming.delete("sel-remove");
      render();
    });
  });
  app.querySelectorAll<HTMLButtonElement>("button.sel-install").forEach((b) =>
    b.addEventListener("click", () => void selectionInstall(b.dataset.arg!))
  );
  document.querySelector("button.sel-remove")?.addEventListener("click", () => void selectionRemove());
  document.querySelector("button.sel-clear")?.addEventListener("click", () => {
    selected.clear();
    confirming.delete("sel-remove");
    render();
  });
  app.querySelectorAll<HTMLButtonElement>("button.retry-kit").forEach((b) =>
    b.addEventListener("click", () => void rescanAll())
  );
  app.querySelectorAll<HTMLButtonElement>("button.kit-authorize").forEach((b) =>
    b.addEventListener("click", () => void kitAuthAction(b.dataset.arg!))
  );
  app.querySelectorAll<HTMLSelectElement>("select.channel").forEach((s) =>
    s.addEventListener("change", () => void changeChannel(s.dataset.arg!, s.value))
  );
  app.querySelectorAll<HTMLSelectElement>("select.bundle").forEach((s) =>
    s.addEventListener("change", () => void changeBundle(s.dataset.arg!, s.value || null))
  );
  document.querySelector<HTMLSelectElement>("#lang-select")?.addEventListener("change", (e) => {
    lang = (e.target as HTMLSelectElement).value;
    localStorage.setItem("crewkit-lang", lang);
    render();
  });
  document.querySelector("#add-kit-toggle")?.addEventListener("click", () => {
    addKitOpen = true;
    render();
    document.querySelector<HTMLInputElement>("#add-kit-url")?.focus();
  });
  document.querySelector("#add-kit-cancel")?.addEventListener("click", () => {
    addKitOpen = false;
    addKitError = "";
    render();
  });
  const input = document.querySelector<HTMLInputElement>("#add-kit-url");
  input?.addEventListener("input", () => {
    addKitUrl = input.value;
    document.querySelector<HTMLButtonElement>("#add-kit-btn")!.disabled = addingKit || !addKitUrl.trim();
  });
  input?.addEventListener("keydown", (e) => {
    if (e.key === "Enter") void addKit();
    if (e.key === "Escape") {
      addKitOpen = false;
      render();
    }
  });
  document.querySelector("#add-kit-btn")?.addEventListener("click", () => void addKit());
}

// --- Actions ---

async function installAppUpdate(): Promise<void> {
  updatingApp = true;
  appUpdateError = "";
  render();
  try {
    // On success the app restarts itself; this call never resolves.
    await invoke("install_app_update");
  } catch (e) {
    logEvents({ step: "App update", client: "crewkit", status: "failed", message: String(e) });
    appUpdateError = String(e);
    updatingApp = false;
    render();
  }
}

async function loadKits(): Promise<void> {
  kits = await invoke<KitCard[]>("list_kits");
}

async function rescanAll(): Promise<void> {
  scanErrors.clear();
  try {
    await loadKits();
    fatalError = null;
    loaded = true;
  } catch (e) {
    fatalError = String(e);
    render();
    return;
  }
  for (const card of kits) {
    if (card.error) continue;
    try {
      scans.set(card.kit.id, await invoke<ScanReport>("scan_kit", { kitId: card.kit.id }));
    } catch (e) {
      scanErrors.set(card.kit.id, String(e));
    }
  }
  render();
}

async function install(kitId: string): Promise<void> {
  installing.add(kitId);
  currentStep = "";
  liveLogCount = 0;
  restartNeeded = [];
  render();
  try {
    const report = await invoke<InstallReport>("install_kit", { kitId });
    if (liveLogCount) logSteps.splice(-liveLogCount);
    logEvents(...report.steps);
    restartNeeded = report.restartNeeded;
    scans.set(kitId, report.scan);
    await loadKits();
  } catch (e) {
    logEvents({ step: "Install", client: "crewkit", status: "failed", message: String(e) });
  }
  installing.delete(kitId);
  render();
}

function mergeRestart(names: string[]): void {
  for (const name of names) {
    if (!restartNeeded.includes(name)) restartNeeded.push(name);
  }
}

/// Run a scoped install or removal. `busyKey` marks a single cell as
/// busy; without it the whole kit shows the progress line instead.
async function applyScoped(
  action: "install" | "remove",
  kitId: string,
  clients: string[] | null,
  items: { kind: string; id: string }[] | null,
  busyKey?: string
): Promise<void> {
  if (busyKey) busy.add(busyKey);
  else {
    working.add(kitId);
    currentStep = "";
  }
  render();
  try {
    const report =
      action === "install"
        ? await invoke<InstallReport>("install_items", { kitId, clients, items })
        : await invoke<InstallReport>("remove_items", { kitId, clients, items: items ?? [] });
    logEvents(...report.steps);
    mergeRestart(report.restartNeeded);
    scans.set(kitId, report.scan);
  } catch (e) {
    logEvents({
      step: action === "install" ? "Install" : "Remove",
      client: "crewkit",
      status: "failed",
      message: String(e),
    });
  }
  if (busyKey) busy.delete(busyKey);
  else working.delete(kitId);
  render();
}

/// A status cell was clicked: install there, or confirm-then-remove there.
async function cellAction(arg: string): Promise<void> {
  const [, kitId, kind, id, colId] = arg.split(SEP);
  const col = COLUMNS.find((c) => c.id === colId);
  const scan = scans.get(kitId);
  if (!col || !scan) return;
  const status = aggregate(itemsFor(scan, kind, id, col.surfaces));
  if (status === "installed") {
    if (!confirming.has(arg)) {
      confirming.add(arg);
      render();
      setTimeout(() => {
        if (confirming.delete(arg)) render();
      }, 4000);
      return;
    }
    confirming.delete(arg);
    await applyScoped("remove", kitId, col.targets, [{ kind, id }], arg);
  } else if (status === "not-installed" || status === "installed-foreign") {
    // Foreign = added outside CrewKit; installing adopts the entry.
    await applyScoped("install", kitId, col.targets, [{ kind, id }], arg);
  }
}

async function ecoInstall(menuKey: string): Promise<void> {
  const [kitId, colId] = menuKey.split(SEP);
  const col = COLUMNS.find((c) => c.id === colId);
  if (!col) return;
  await applyScoped("install", kitId, col.targets, null);
}

async function ecoRemove(menuKey: string): Promise<void> {
  const key = `eco-remove${SEP}${menuKey}`;
  if (!confirming.has(key)) {
    confirming.add(key);
    render();
    setTimeout(() => {
      if (confirming.delete(key)) render();
    }, 4000);
    return;
  }
  confirming.delete(key);
  openMenu = null;
  const [kitId, colId] = menuKey.split(SEP);
  const col = COLUMNS.find((c) => c.id === colId);
  const card = kits.find((k) => k.kit.id === kitId);
  if (!col || !card) return;
  await applyScoped("remove", kitId, col.targets, kitItems(card));
}

function groupSelected(): Map<string, { kind: string; id: string }[]> {
  const byKit = new Map<string, { kind: string; id: string }[]>();
  for (const key of selected) {
    const [kitId, kind, id] = key.split(SEP);
    const list = byKit.get(kitId) ?? [];
    list.push({ kind, id });
    byKit.set(kitId, list);
  }
  return byKit;
}

async function selectionInstall(target: string): Promise<void> {
  const clients = target === "both" ? null : (COLUMNS.find((c) => c.id === target)?.targets ?? null);
  for (const [kitId, items] of groupSelected()) {
    await applyScoped("install", kitId, clients, items);
  }
  selected.clear();
  render();
}

async function selectionRemove(): Promise<void> {
  if (!confirming.has("sel-remove")) {
    confirming.add("sel-remove");
    render();
    setTimeout(() => {
      if (confirming.delete("sel-remove")) render();
    }, 4000);
    return;
  }
  confirming.delete("sel-remove");
  for (const [kitId, items] of groupSelected()) {
    await applyScoped("remove", kitId, null, items);
  }
  selected.clear();
  render();
}

/// Sign in to a kit published behind a login: the browser opens, and the
/// list refreshes once the session lands so the kit fills in.
async function kitAuthAction(kitId: string): Promise<void> {
  busy.add(`auth:${kitId}`);
  render();
  try {
    await invoke("authorize_kit", { kitId });
  } catch (e) {
    logEvents({ step: `authorize kit ${kitId}`, client: "crewkit", status: "failed", message: String(e) });
  }
  busy.delete(`auth:${kitId}`);
  await rescanAll();
}

async function authAction(command: "authorize" | "deauthorize", serverId: string): Promise<void> {
  busy.add(`auth:${serverId}`);
  render();
  try {
    await invoke(command, { serverId });
  } catch (e) {
    logEvents({ step: `${command} ${serverId}`, client: "crewkit", status: "failed", message: String(e) });
  }
  busy.delete(`auth:${serverId}`);
  await rescanAll();
}

async function removeItem(key: string): Promise<void> {
  if (!confirming.has(key)) {
    confirming.add(key);
    render();
    setTimeout(() => {
      if (confirming.delete(key)) render();
    }, 4000);
    return;
  }
  confirming.delete(key);
  busy.add(key);
  render();
  const [kitId, kind, id] = key.split(" ");
  try {
    const report = await invoke<InstallReport>("remove_item", { kitId, kind, id });
    logEvents(...report.steps);
    restartNeeded = report.restartNeeded;
    scans.set(kitId, report.scan);
  } catch (e) {
    logEvents({ step: `Remove ${id}`, client: "crewkit", status: "failed", message: String(e) });
  }
  busy.delete(key);
  render();
}

async function removeKit(kitId: string): Promise<void> {
  const key = `kit ${kitId}`;
  if (!confirming.has(key)) {
    confirming.add(key);
    render();
    setTimeout(() => {
      if (confirming.delete(key)) render();
    }, 4000);
    return;
  }
  confirming.delete(key);
  try {
    await invoke("remove_kit", { kitId });
    logEvents({ step: "Remove kit", client: "crewkit", status: "ok", message: kitId });
    scans.delete(kitId);
    await rescanAll();
  } catch (e) {
    logEvents({ step: `Remove kit ${kitId}`, client: "crewkit", status: "failed", message: String(e) });
    render();
  }
}

async function changeChannel(kitId: string, channel: string): Promise<void> {
  try {
    await invoke("set_channel", { kitId, channel });
  } catch (e) {
    logEvents({ step: `Channel ${channel}`, client: "crewkit", status: "failed", message: String(e) });
  }
  await rescanAll();
}

async function changeBundle(kitId: string, bundle: string | null): Promise<void> {
  try {
    await invoke("set_bundle", { kitId, bundle });
  } catch (e) {
    logEvents({ step: "Bundle", client: "crewkit", status: "failed", message: String(e) });
  }
  await rescanAll();
}

async function addKit(): Promise<void> {
  const url = addKitUrl.trim();
  if (!url) return;
  addingKit = true;
  addKitError = "";
  render();
  try {
    await invoke("add_kit", { url });
    logEvents({ step: "Add kit", client: "crewkit", status: "ok", message: url });
    addKitUrl = "";
    addKitOpen = false;
    await rescanAll();
  } catch (e) {
    logEvents({ step: "Add kit", client: "crewkit", status: "failed", message: String(e) });
    addKitError = String(e);
  }
  addingKit = false;
  render();
}

async function main(): Promise<void> {
  // The journal loads before anything can write to it. A successful app
  // update restarts the app, so it can only be recorded here: the file
  // carrying a different writer's version means an update landed.
  const stored = await invoke<EventLogFile>("load_event_log").catch(() => null);
  if (stored) {
    logSteps = stored.entries.filter((e) => e && typeof e.atMs === "number");
    if (stored.appVersion && stored.appVersion !== __APP_VERSION__) {
      logEvents({
        step: "App update",
        client: "crewkit",
        status: "ok",
        message: `v${stored.appVersion} → v${__APP_VERSION__}`,
      });
    } else {
      // Stamp the current version even when nothing else changed.
      persistLog();
    }
  }

  await listen<StepReport>("install-step", (event) => {
    currentStep = `${event.payload.step}…`;
    if (installing.size) {
      logEvents(event.payload);
      liveLogCount++;
      render();
    } else if (working.size) {
      // Scoped runs append the report's steps once at the end; live
      // events only refresh the progress line.
      render();
    }
  });
  await listen("kits-updated", () => void rescanAll());
  await listen("kits-changed", () => void rescanAll());
  await listen<string>("app-update-available", (event) => {
    appUpdate = event.payload;
    render();
  });
  await listen<string>("deep-link-add-kit", (event) => {
    addKitOpen = true;
    addKitUrl = event.payload;
    render();
    document.querySelector("#add-kit-url")?.scrollIntoView({ behavior: "smooth" });
  });

  try {
    await rescanAll();
  } catch (e) {
    fatalError = String(e);
    render();
  }
  try {
    appUpdate = await invoke<string | null>("check_app_update");
    if (appUpdate) render();
  } catch {
    // offline is fine
  }
}

// Any click outside an open column menu closes it (standard pull-down
// behavior); clicks inside .col-wrap are the menu's own controls.
document.addEventListener("click", (e) => {
  if (openMenu && !(e.target as HTMLElement).closest(".col-wrap")) {
    openMenu = null;
    render();
  }
});

// Window dragging: the hidden-title window is moved by its toolbar.
// (Explicit handler in addition to data-tauri-drag-region, so dragging
// works regardless of the injected-script path.)
document.addEventListener("mousedown", (e) => {
  const target = e.target as HTMLElement;
  if (e.buttons !== 1 || !target.closest(".toolbar")) return;
  if (target.closest("button, select, input, a")) return;
  if (e.detail === 2) {
    void getCurrentWindow().toggleMaximize();
  } else {
    void getCurrentWindow().startDragging();
  }
});

void main();
