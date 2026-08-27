# CrewKit kit manifest — specification v1.0

This specification is licensed under [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/).
Anyone may implement it — publishers, alternative installers, client vendors — with attribution.

## Overview

A **kit** is the unit a publisher ships: a named set of AI skills/plugins and MCP servers
that an installer (such as the CrewKit app) puts into every AI client on a user's machine.
A published kit is two files on any static hosting:

```
https://example.com/kits/acme.json        the manifest (this spec)
https://example.com/kits/acme.json.sig    detached ed25519 signature, base64,
                                          over the manifest's exact bytes
```

No central marketplace, no gatekeeper: every publisher hosts their own kit.

## Versioning of this spec

A manifest declares the spec version it targets in its `spec` field, as
`"MAJOR.MINOR"`. A manifest without the field is treated as `"1.0"`.

- **Minor revisions only add optional fields.** Installers MUST ignore fields they
  do not recognize — that is how a 1.0 installer stays compatible with a 1.1 manifest.
  (Consequently, typos in field names are not caught by installers; validate manifests
  with publisher-side tooling.)
- **Major revisions may change meaning.** An installer MUST reject a manifest whose
  spec major version it does not support, with a message telling the user to update.

## Transport security

Every URL in this system MUST use `https`: the manifest URL, `channels`, artifact
URLs, MCP server URLs, `docs`, and telemetry URLs. Installers MUST reject manifests
and refuse fetches that violate this. The only exception is loopback
(`http://localhost`, `http://127.0.0.1`, `http://[::1]`) so publishers can test
against a local server during development.

## Trust model

- The publisher's ed25519 **public key travels inside the manifest** (`publisherKey`).
- The installer verifies the detached signature against that key on every fetch.
- On first add, the installer **pins** the key (trust on first use). A later manifest
  signed with a different key must be rejected — a compromised CDN cannot substitute
  a different publisher. Rotating a key legitimately requires users to re-add the kit.
- Every plugin artifact is pinned by **sha256**; a download that does not match is rejected.

## Manifest fields

```jsonc
{
  "spec": "1.0",                      // recommended · spec version this manifest targets
                                      //   (absent = "1.0"; unknown major → reject)
  "id": "acme",                       // required · stable machine id, lowercase-hyphen
  "name": "Acme AI Kit",              // required · human name
  "version": "1.2.0",                 // recommended · version of the manifest itself
  "publisher": "Acme Corp",           // required · human publisher name
  "publisherKey": "<base64 ed25519>", // required for URL kits · signing key
  "homepage": "https://…",            // optional
  "marketplaceName": "acme",          // required · plugin ids become <plugin>@<marketplaceName>;
                                      //   the publisher's namespace — MUST be unique per kit
                                      //   (installers reject a second kit reusing the name)

  // Optional release channels: alternate manifest URLs, absolute (https) or
  // relative to this manifest's URL. The installer lets the user switch.
  "channels": { "stable": "/kits/acme.json", "beta": "/kits/acme-beta.json" },

  // Optional, ALWAYS disclosed to the user before anything is sent:
  // installers POST anonymous install reports (kit id/version, item
  // versions and install statuses per client, OS, app version, random
  // per-machine id) to `endpoint`. `notice` is a human-readable page
  // describing what is collected.
  "telemetry": { "endpoint": "https://…", "notice": "https://…" },

  // Optional role bundles: named subsets a user can choose to install.
  "bundles": [
    { "id": "email-team", "displayName": "Email team",
      "plugins": ["mail-writer"], "mcpServers": ["acme-mcp"] }
  ],

  "mcpServers": [
    {
      "id": "acme-mcp",                    // required · lowercase-hyphen
      "url": "https://mcp.acme.com/mcp",   // required · https endpoint
      "transport": "http",                 // optional · wire protocol; default "http"
                                           //   = MCP Streamable HTTP, the only value
                                           //   defined by spec 1.0. An entry with an
                                           //   unrecognized transport MUST be skipped
                                           //   with a warning — never fail the kit.
      "auth": "oauth",                     // optional · "oauth" (default): installer
                                           //   runs the client-side OAuth flow;
                                           //   "none": open endpoint, no authorize step
      "docs": "https://…",                 // optional · human documentation page
      "displayName": "Acme MCP",           // optional · UI alias
      "description": "…",                  // optional
      "remove": false                      // optional · true retires the server:
                                           //   installers clean it out of every client
    }
  ],

  "plugins": [
    {
      "name": "mail-writer",               // required · lowercase-hyphen
      "artifact": {                        // required for URL kits
        "url": "/skills/mail-writer.zip",  //   https, or manifest-relative
        "sha256": "…"                      //   hex digest of the zip
      },
      "version": "2.0.1",                  // optional · advertised version
      "displayName": "Mail Writer",        // optional · UI alias
      "description": "…",                  // optional
      "remove": false                      // optional · true retires the plugin
    }
  ]
}
```

Secrets never belong in a manifest. There is deliberately no field for API keys,
tokens, or custom auth headers: manifests are public, cacheable documents, and
credentials are obtained per user via OAuth (`"auth": "oauth"`) or not at all.

## Plugin payload

The artifact zip may contain any of:

1. a **Claude plugin** — `.claude-plugin/plugin.json` + `skills/*/SKILL.md`;
2. a **Codex plugin** — `.codex-plugin/plugin.json` + `skills/*/SKILL.md`;
3. a **bare Agent Skill** — `SKILL.md` at the root (plus `scripts/`, `references/`, `assets/`).

Installers normalize all three into a dual-manifest plugin that both the Claude and the
Codex ecosystems install from one staged marketplace directory. Bare skills are wrapped
into a single-skill plugin. Skill frontmatter is validated (required `name` and
`description`, lowercase-hyphen names); keys a host does not recognize are surfaced as
partial-support warnings, never silently dropped. For ChatGPT, `agents/openai.yaml` UI
metadata is generated when the skill does not ship one.

## MCP delivery

MCP servers are remote Streamable HTTP endpoints (`"transport": "http"`) with OAuth
handled entirely client-side. CrewKit wires them through its local stdio bridge
(`crewkit-bridge <id>`), so the user authorizes each server once and every AI client
shares the session. Other installers may choose any equivalent mechanism. Future spec
minors may define additional transports; installers skip entries they cannot serve.

## Installer obligations

An installer implementing this spec MUST:

1. reject manifests targeting a spec major version it does not support, and ignore
   unknown fields otherwise;
2. require https for every URL (loopback excepted) — the manifest URL included;
3. verify the signature before using a manifest, and pin the publisher key on first add;
4. verify artifact sha256 digests before unpacking;
5. modify only its own entries in client configs — never a user's manual configuration;
6. write configs atomically and snapshot every file before modifying it;
7. be idempotent: repeat installs update or skip, never duplicate;
8. disclose telemetry to the user before any report is sent;
9. skip MCP servers whose transport it does not support, with a visible warning;
10. remove retired (`"remove": true`) items it previously installed, including cached
    credentials for retired MCP servers.
