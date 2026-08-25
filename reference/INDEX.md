# `reference/` — what's here and where

Vendored upstream documentation. **Nothing in this directory is ai-kanban's own work** — it is
read-only reference material, kept in-repo so sessions can consult it without network access.

Two independent sets:

- **Claude Code docs** (root level) — the host we build a plugin/skill/hook integration for
- **`mcp-docs/`** — the full Model Context Protocol spec repo (docs, schema, and SEPs)

Sizes are given because several files are large enough that reading them whole is a real cost.
Prefer `grep -n '^#' <file>` to find a section, then read that range.

---

## Load-bearing for this project

Read these before touching the corresponding part of the design.

| Topic | File | Why |
|---|---|---|
| Tool surface + response shape | `../docs/plan.md`, and the external `ToolDesign.md` it draws from | The design law the whole project follows |
| MCP server implementation | `mcp-docs/develop/build-server.mdx` | 3106 lines — the primary build guide |
| Protocol model | `mcp-docs/learn/architecture.mdx`, `learn/server-concepts.mdx` | Lifecycle, capability negotiation, primitives |
| Wire types | `mcp-docs/schema/schema.ts` | Authoritative TypeScript definitions |
| **Project resolution** | `mcp-docs/seps/2577-…md` | **Deprecates `roots/list`** — see the warning below |
| Adoption mechanism | `hooks.md`, `skills.md`, `plugins.md` | The deferred decision in `docs/adoption.md` |

> ### ⚠️ `roots/list` is deprecated
>
> **SEP-2577** (Status: Final, 2026-04-14) deprecates Roots, Sampling, and Logging. Roots remains
> functional for at least a year past the June 2026 spec version, so nothing breaks today — but it
> must not be the primary mechanism for project resolution.
>
> The SEP names the replacements: *"working directory context can be provided through tool
> parameters, resource URIs, server configuration, or environment variables — all of which are more
> explicit."* For us that means `CLAUDE_PROJECT_DIR` first, process cwd second, `roots/list` only
> opportunistically.
>
> Also read **SEP-2575 (Make MCP Stateless)** and **SEP-2567 (Sessionless MCP)** before
> implementing — both push against ambient per-connection context, which auto-resolution relies on.

---

## Claude Code docs (root level)

| File | Lines | Contents |
|---|---:|---|
| `hooks.md` | 3532 | Hook reference. Lifecycle, config, matchers, and **every hook event** — `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `FileChanged`, and ~25 more. Prompt-based and agent-based hooks, async hooks. |
| `mcp.md` | 1405 | Using MCP *from* Claude Code: installing servers, the four transports, installation scopes, `CLAUDE_PROJECT_DIR`, plugin-provided servers, `.mcp.json`. |
| `plugins-reference.md` | 1332 | Plugin manifest schema, component paths, env vars, CLI commands, directory layout, debugging. |
| `skills.md` | 1054 | Skills: frontmatter reference, content lifecycle, `allowed-tools`, arguments and substitutions, subagent execution, dynamic context injection. |
| `tools-reference.md` | 532 | Built-in tool behavior and limits (Bash, Read, Edit, Agent, Monitor, …). |
| `plugins.md` | 473 | Plugin authoring guide — quickstart, structure, bundling skills/hooks/MCP, distribution. |

---

## `mcp-docs/` — Model Context Protocol spec repo

### Getting started & concepts

| File | Lines | Contents |
|---|---:|---|
| `getting-started/intro.mdx` | 65 | What MCP is. Start here if new. |
| `learn/architecture.mdx` | 568 | Layers, lifecycle, capability negotiation, message flow. |
| `learn/server-concepts.mdx` | 285 | Tools, resources, prompts — the server-side primitives. |
| `learn/client-concepts.mdx` | 269 | Sampling, roots, elicitation — the client-side primitives. |
| `learn/versioning.mdx` | 68 | How spec versions are named and negotiated. |

### Building

| File | Lines | Contents |
|---|---:|---|
| `develop/build-server.mdx` | 3106 | **The main build guide.** Multi-language walkthrough. |
| `develop/build-client.mdx` | 2570 | Client-side counterpart. |
| `develop/connect-local-servers.mdx` | 335 | stdio transport, local server wiring. |
| `develop/clients/client-best-practices.mdx` | 308 | How well-behaved clients act — useful for predicting host behavior. |
| `develop/connect-remote-servers.mdx` | 158 | Remote/HTTP transport. |
| `develop/build-with-agent-skills.mdx` | 107 | Skills alongside MCP. |
| `sdk.mdx` | 55 | SDK list per language, with tiering. |

### Schema (authoritative wire format)

| File | Size | Contents |
|---|---:|---|
| `schema/schema.ts` | 3197 lines | **TypeScript definitions — the source of truth for every message type.** |
| `schema/schema.json` | 181 KB | JSON Schema, generated from the above. |
| `schema/schema.mdx` | 109 lines | How to read the schema. |
| `schema/examples/` | 88 dirs | Concrete request/response JSON per type. Fastest way to see a real payload — e.g. `CallToolResult/`, `ListRootsResult/`, `Tool/`. |

### Tooling

| File | Lines | Contents |
|---|---:|---|
| `tools/debugging.mdx` | 389 | Debugging MCP servers. |
| `tools/inspector.mdx` + `inspector/` (7 files) | ~1430 | MCP Inspector — TUI, web, and CLI clients, config, auth, recipes. **The way to exercise our server without a live agent.** |

### Security

| File | Lines | Contents |
|---|---:|---|
| `tutorials/security/authorization.mdx` | 1201 | OAuth flows for MCP. |
| `tutorials/security/security_best_practices.mdx` | 984 | Threat model and hardening. |

> Mostly relevant to *remote* servers. A local stdio server has a much smaller attack surface —
> revisit when `ai-kanban serve` (HTTP) is built.

### `seps/` — Specification Enhancement Proposals (41 files, all Status: Final)

Rationale and history behind protocol decisions. Reach for these when you need to know *why* the
protocol works a way it does, or where it is heading.

Most relevant to ai-kanban:

| SEP | Subject |
|---|---|
| **2577** | **Deprecates Roots, Sampling, Logging** — see warning above |
| **2575** | Make MCP Stateless |
| **2567** | Sessionless MCP via Explicit State Handles |
| 2243 | HTTP header standardization for Streamable HTTP |
| 986 | Tool name format rules |
| 1303 | Input validation errors as tool execution errors |
| 2164 | Standard "resource not found" error code |
| 2106 / 1613 | JSON Schema 2020-12 as the dialect for `inputSchema`/`outputSchema` |
| 973 | Extra metadata on tools, resources, prompts |
| 2549 | TTL for list results |
| 1686 / 2663 | Tasks (core, then moved to an extension) |
| 2133 | Extensions mechanism |
| 1865 | MCP Apps — interactive UIs |

Remaining SEPs cover OAuth details (985, 990, 991, 1046, 2207, 2468), elicitation (1034, 1036,
1330), governance and process (932, 994, 1302, 1730, 1850, 2085, 2148, 2149, 2484, 2596), and
assorted protocol changes (414, 1024, 1319, 1577, 1699, 2260, 2322).

`seps/README.md` and `seps/TEMPLATE.md` describe the SEP process itself.
