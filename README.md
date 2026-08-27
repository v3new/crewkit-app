# CrewKit

**One button to install AI skills, plugins and MCP servers into every AI client on your machine.**

A publisher describes a set once — a signed *kit* manifest on any static hosting. The user clicks **Install kit** — and the plugins and MCP servers land in Claude Code, Claude Cowork, Claude Desktop, Codex and ChatGPT Desktop, correctly registered through each client's own mechanisms. The user is *not* a developer: zero terminal, zero manual config editing.

> **Status: 1.0 on macOS.** Signed kit manifests by URL, release channels, `crewkit://` deep links, menu-bar tray with background updates, Keychain-stored OAuth sessions, role bundles, disclosed per-kit telemetry, EN/RU/ES/ZH UI, rollback. The app starts clean — users add publisher kits by URL or deep link. Windows and MDM are the remaining roadmap items. See the [roadmap](#roadmap).

## How it works

CrewKit does not automate UI clicks and never touches OAuth tokens. It uses only the clients' own automatic surfaces, all verified on live clients:

| Mechanism | Client | How |
|---|---|---|
| Plugins | Claude Code / Cowork | staged local marketplace + `claude plugin marketplace add` / `claude plugin install` |
| Plugins | Codex / ChatGPT Desktop | the same staged marketplace + `codex plugin marketplace add` / `codex plugin add` |
| MCP servers | every client | one identical stdio entry per client: `crewkit-bridge <server-id>` |

### crewkit-bridge: authorize once, use everywhere

Remote MCP servers are not wired into clients directly. Instead, every client launches
**crewkit-bridge** — a small Rust binary (no Node, no npx) that speaks MCP stdio on one
side and Streamable HTTP on the other, and **owns the OAuth session at the CrewKit level**:

- Full OAuth 2.1: RFC 9728/8414 discovery (with `openid-configuration` fallback), RFC 7591
  dynamic client registration, authorization-code + PKCE in the system browser, refresh.
- One login per server — the cached session is shared by Claude Code, Cowork, Claude
  Desktop, Codex and ChatGPT Desktop. No more logging in N clients × M servers.
- Concurrent client startups produce **one** browser tab (cross-process login lock).
- Pure passthrough proxy: messages are forwarded verbatim (sessions, protocol version and
  SSE streams handled at the transport layer), so the bridge does not lag protocol changes.
- Solves stdio-only clients for free: Claude Desktop's local config rejects remote HTTP
  entries (verified against the app's own config schema) — with the bridge it just works.
- Sessions live in the macOS Keychain (service "CrewKit MCP"; `0600` file fallback)
  and are never written into client configs or logs.
- Client entries reference a stable path (`…/CrewKit/bin/crewkit-bridge`), and server
  URLs live in CrewKit's `servers.json` — URL or channel changes never touch client configs.

Verified live end-to-end: browser OAuth against a real MCP server, then `initialize` and
`tools/list` (47 tools) proxied over stdio.

Two findings make the "zero terminal" promise real:

- **Client CLIs ship inside the desktop apps.** `codex` lives in `ChatGPT.app/Contents/Resources/`, and Claude's `claude` binary ships with Claude Desktop. CrewKit finds and drives the bundled binaries even when nothing is on `PATH`.
- **One marketplace directory serves both ecosystems.** Claude reads `.claude-plugin/marketplace.json`, Codex reads `.agents/plugins/marketplace.json`; each staged plugin carries both manifests. Kit payloads are normalized on the way in — a zip may contain a Claude plugin, a Codex plugin, or a bare Agent Skill folder, which gets wrapped into a single-skill plugin.

## Kit manifest

A kit is one JSON manifest published at a URL, with a detached ed25519 signature and
sha256-pinned artifact downloads (live example:
[crewkit.v3new.dev/kit/mindbox-int-csm.json](https://crewkit.v3new.dev/kit/mindbox-int-csm.json)).
The full format is an open spec: **[docs/kit-spec.md](docs/kit-spec.md)** (CC BY 4.0).
Highlights:

- `spec` — the spec version the manifest targets (`"1.0"`); unknown fields from newer
  minors are ignored, an unsupported major is rejected with an update prompt.
- **https everywhere** — every URL in the manifest (and the manifest URL itself) must be
  https; loopback is exempt for development.
- `publisherKey` + `<manifest>.sig` — ed25519 signature, key pinned on first add
  (a compromised CDN cannot swap publishers).
- `channels` — alternate manifest URLs (stable/beta); the user switches in the UI.
- `bundles` — role-based subsets of the kit.
- `telemetry` — per-kit install reporting, always disclosed on the kit card.
- MCP entries carry `transport` (`"http"` = Streamable HTTP; unsupported transports are
  skipped with a warning), `auth` (`"oauth"` default, `"none"` for open endpoints) and
  `docs`. Secrets deliberately have no place in a manifest.
- `displayName` — human-facing alias shown in the UI instead of the raw id.
- `remove: true` — retire an item: the installer cleans it out of every client (and drops
  the MCP server's cached session) instead of installing it. Items can also be removed
  per-item from the UI.

Publisher tooling ships in the CLI: `crewkit kit keygen <name>` and
`crewkit kit sign <manifest> <secret-key-file>`. Users add kits via the in-app URL field
or a deep link: `crewkit://add?kit=https://crewkit.v3new.dev/kit/mindbox-int-csm.json`.

Every staged skill passes the `skill-translate` check: frontmatter is validated against a
declarative mapping table ([adapters/frontmatter-map.json](adapters/frontmatter-map.json)),
unrecognized host keys surface as partial-support notes in the install log, and
`agents/openai.yaml` (ChatGPT UI metadata) is generated for skills that don't ship one.

## Hard rules of the installer

1. **Merge, never overwrite.** CrewKit creates and updates only its own entries (marked `"_managedBy": "crewkit"` where the format allows, tracked in CrewKit's own state file otherwise). Entries the user configured by hand are never touched.
2. **Atomic writes.** Temp file + rename; a client can never observe a half-written config.
3. **Snapshot before every write** of every touched file, for manual rollback.
4. **Idempotent.** Reinstalling updates or skips — never duplicates: when the kit ships a
   newer plugin version, the installer updates it in place (`claude plugin update`, codex
   snapshot reinstall); otherwise the step is skipped with the installed version reported.
5. **Secrets stay out of configs and logs.** OAuth is owned by crewkit-bridge: sessions
   live in the macOS Keychain (service "CrewKit MCP"; 0600 file fallback elsewhere),
   never in client configs, never in logs.
6. **Honest restart guidance.** Clients that need a restart are listed; CrewKit doesn't kill apps.
7. **Adapters are data, not code.** Every client is described by a declarative JSON file in [adapters/](adapters/) — paths, CLI discovery globs, state files.

## Repository layout

```
crates/crewkit-core/   Rust engine: detect → kits → stage marketplace → install → inventory
crates/crewkit-bridge/ Local stdio⇄HTTP MCP proxy with CrewKit-level OAuth
crates/crewkit-cli/    `crewkit` command line sharing the same engine
desktop/               Tauri 2 app: tray, deep links, background updates (vanilla TS UI)
adapters/              Declarative client adapters (data, not code)
docs/                  Kit spec (CC-BY), spike findings
```

## Development

Prerequisites: Rust (stable), Node 20+.

```bash
# run the test suite (sandboxed, never touches your real configs)
cargo test -p crewkit-core

# full end-to-end against the real client CLIs, still sandboxed
CREWKIT_E2E=1 cargo test -p crewkit-core --test e2e -- --nocapture

# command line (same engine as the app)
cargo run -p crewkit-cli -- scan
# install needs the kit payload zips; point CREWKIT_ASSETS at a dir with skills/
CREWKIT_ASSETS=~/path/to/published-kit cargo run -p crewkit-cli -- install
cargo run -p crewkit-cli -- remove mcp mindbox-mcp-beta

# run the app in dev mode
cd desktop && npm install && npm run tauri dev

# build CrewKit.app + .dmg
cd desktop && npm run tauri build
```

Note: Tauri's dmg bundler drives Finder via AppleScript and fails in headless
sessions; if that happens, wrap the built `CrewKit.app` yourself:
`hdiutil create -volname CrewKit -srcfolder <dir with CrewKit.app> -format UDZO CrewKit.dmg`.

## Roadmap

- **1.0 (current, macOS):** signed URL manifests with key pinning, stable/beta channels,
  `crewkit://add` deep links, honest inventory, idempotent installs, tray + background
  updates, in-place signed app self-update (Tauri updater; kits and the app both checked
  every 2 hours), disclosed per-kit telemetry, EN/RU/ES/ZH UI, snapshots +
  `crewkit rollback`, Keychain sessions, role bundles. Open source: Apache-2.0 code,
  [CC-BY manifest spec](docs/kit-spec.md).
- **Next:** Windows build, MDM policies.

## License

[Apache-2.0](LICENSE)
