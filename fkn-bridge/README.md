# FKN Codex bridge prototype

This directory proves the first architectural invariant of the project without
touching the user's installed Codex or `~/.codex`:

```text
local Responses bridge
        -> bundled/fork checkout Codex
        -> native exec_command
        -> function_call_output
        -> local Responses bridge
```

Run from the repository root:

```bash
node fkn-bridge/smoke.mjs
```

The script creates a temporary `CODEX_HOME` and temporary workspace, starts a
local Responses endpoint on a random loopback port, then launches this checkout's
`codex-cli` through Cargo with that endpoint configured as its model provider.

The first bridge response asks Codex to execute its native `exec_command` tool
with `pwd`. The second `/v1/responses` request must contain the corresponding
`function_call_output`. The bridge then completes the turn and prints `PASS`.

No OpenAI model inference is used by this smoke test.

## `fkn-codex` / `fkn-codex-2` TUI

The user-facing entry points are `fkn-codex` and the parallel `fkn-codex-2` binary.
The original launcher keeps using `fkn-codex-bridge`; the second launcher uses the
separate `fkn-codex-bridge-2`, so the existing build stays available while the new
bridge can be launched separately:

```bash
cd /path/to/project
fkn-codex-2
```

The current directory becomes the Codex workspace. Connection settings are app-wide,
not project-local: create one dedicated Secure MCP Tunnel for FKN Codex, save its
Tunnel ID and runtime API key once, then reuse that tunnel while launching
`fkn-codex` from different project directories.

The compact home screen intentionally has no activity/task/agent panel. It exposes:

- **Start** — starts the local bridge + hidden bundled Codex, waits for the native
  Codex tool registry to be ready, then starts the official OpenAI `tunnel-client`.
- **Pause** — stops only `tunnel-client`; the local bridge and hidden Codex process
  stay alive so shell sessions, `cua_repl`, Browser, and Computer Use state are not
  discarded.
- **Resume** — starts a new `tunnel-client` process against the same live local MCP
  runtime.
- **Settings** — dedicated tunnel credentials, Codex sandbox permission, and whether
  Browser/Computer Use should be bootstrapped from ChatGPT Desktop.
- **Diagnostics** — project, process state, local MCP URL, tunnel health URL, app
  directories, resolved bridge/Codex/tunnel binaries, and the latest runtime event.

Quitting the TUI stops both the tunnel and the local runtime it started.

The runtime API key is stored separately from `config.json` and is never rendered
back into the TUI. On Unix the secret file is written with mode `0600`.

Default app state locations:

```text
macOS:   ~/Library/Application Support/FKN Codex/
Windows: %APPDATA%/FKN Codex/
Linux:   $XDG_CONFIG_HOME/fkn-codex/ or ~/.config/fkn-codex/
```

For development, binary resolution supports `FKN_CODEX_BRIDGE_BIN`, `FKN_CODEX_BIN`,
and `FKN_TUNNEL_CLIENT_BIN`. A packaged release can place `fkn-codex-bridge`, the
forked `codex` binary, and `tunnel-client` beside `fkn-codex`; sibling binaries are
preferred automatically.

The TUI uses the supported Secure MCP Tunnel foreground daemon path roughly as:

```text
tunnel-client run
  --control-plane.tunnel-id <dedicated FKN tunnel>
  --control-plane.api-key file:<secret>
  --mcp.server-url url=http://127.0.0.1:<ephemeral>/mcp,channel=main
  --health.listen-addr 127.0.0.1:0
  --health.url-file <app state>/tunnel-health.url
```

The local MCP listener always binds loopback on an ephemeral port; it is not exposed
directly to the public internet.

## MCP bridge prototype

The Rust prototype exposes both sides of the bridge from one process:

```text
http://127.0.0.1:8787/mcp
http://127.0.0.1:8787/v1/responses
```

It launches the Codex binary from this checkout with a separate `CODEX_HOME`
and the local Responses endpoint configured as its model provider. The MCP side
publishes the active Codex registry as ordinary MCP tools:

- `codex_skills_list` — proxies Codex app-server `skills/list` for the configured workspace.
- `codex_skill_get` — resolves a skill through that native catalog and returns its full `SKILL.md`; arbitrary paths are not accepted.
- Native top-level tools keep their names, including `exec_command`, `write_stdin`,
  `apply_patch`, and `view_image`.
- Namespaced tools use `<namespace>__<tool>`, for example
  `mcp__cua_repl__js`.

The bridge copies native descriptions and JSON schemas into `tools/list`; the model
does not need to inspect an inventory or provide call-type and namespace metadata.
Only tools present in the active hidden Codex registry are advertised. The bridge
declares `tools.listChanged` and emits the standard notification when a restarted
runtime publishes a different registry.

Codex text/image output items are projected to native MCP content blocks, so a
`view_image` result or a CUA screenshot reaches the MCP client as an actual image.
Image base64 is not copied into `structuredContent`; that field contains only compact
content counts for image-bearing results.

### Tool metadata contract

Native function descriptions and parameter schemas are preserved from the active
Responses request. Native custom tools are exposed with one `input` string because
MCP tool calls use JSON objects while Codex custom tools use freeform input.

For Browser/Chrome screenshots, the controller should follow the bound browser-tab
documentation and use the browser API's native image path, for example
`await nodeRepl.emitImage(await tab.screenshot())`. Browser screenshots returned by
`cua_repl` are converted into native MCP image content; undocumented
runtime-specific screenshot helpers are not interchangeable with a bound browser tab.

## Browser + Computer Use

Browser/Computer Use discovery is isolated behind `DesktopRuntimeLocator` in
`src/desktop_runtime.rs`. The bridge and MCP layer do not contain platform-specific
Desktop paths.

Current platform state:

- macOS: implemented and E2E-tested. The locator checks the explicit `--chatgpt-app`
  override first, then `/Applications/ChatGPT.app`, then `~/Applications/ChatGPT.app`.
- Windows: the target compiles and has a separate locator/materialization boundary,
  but Windows ChatGPT Desktop discovery and marketplace materialization are not yet
  implemented or claimed to work.

Adding Windows support therefore only requires implementing the Windows branch in
`DesktopRuntimeLocator`; the Codex plugin bootstrap, MCP API, skills API, and direct
tool projection remain unchanged.

On macOS the bridge bootstraps the unified Computer Use runtime automatically from
the installed `/Applications/ChatGPT.app`. It does not require Browser-specific
bridge tools or any manual Codex/plugin setup by the user.

The bridge materializes Codex's reserved bundled marketplace at the managed path
`$CODEX_HOME/.tmp/bundled-marketplaces/openai-bundled`, backed by the plugin bundle
inside ChatGPT.app. It then uses Codex's own CLI mutations to register the marketplace
and install/enable exactly these native plugins:

- `browser@openai-bundled`
- `chrome@openai-bundled`
- `unified-computer-use@openai-bundled`

Codex itself owns the resulting isolated `config.toml` and plugin cache. This makes
plugin skills and hooks visible through Codex's native loaders; for example,
`skills/list` exposes `browser:control-in-app-browser` and `chrome:control-chrome`
with their real plugin IDs.

At startup the bridge discovers the bundled `node`, `node_repl`, `@oai/cua-repl`,
`@oai/browser-desktop`, and `@oai/sky` resources from ChatGPT.app and injects the
app-managed `cua_repl` transport configuration into the hidden Codex process with
`-c` overrides. The native plugin installation supplies Codex's skills/hooks/plugin
metadata; the small runtime override supplies the desktop-managed executable paths
that are intentionally not hard-coded in the plugin manifest itself. The `CODEX_HOME`
visible to all of it remains the bridge's isolated home.

The external controller sees the CUA constructor directly when that runtime loaded:

```text
mcp__cua_repl__js(code="...")
  -> browser or native Computer Use action
```

The bridge advertises the Chrome backend. It does not advertise the unavailable IAB
backend to CUA.

Pass `--disable-cua` to run without Browser / Computer Use, or `--chatgpt-app`
to override Desktop runtime discovery. If ChatGPT Desktop is absent, startup
fails with an explicit install/disable message instead of silently exposing a broken
Browser/Computer Use runtime.

Build the forked Codex and bridge:

```bash
cd codex-rs
cargo build -p codex-cli --bin codex
cd ../fkn-bridge
cargo build --bin fkn-codex --bin fkn-codex-bridge
cargo build --bin fkn-codex-2 --bin fkn-codex-bridge-2
```

Start the bridge against a workspace:

```bash
./target/debug/fkn-codex-bridge \
  --workspace /path/to/project \
  --codex-bin ../codex-rs/target/debug/codex \
  --codex-home /tmp/fkn-codex-home
```

Then, in another terminal, exercise the complete generic MCP path with the local smoke client:

```bash
./target/debug/fkn-mcp-smoke
```

The generic smoke covers `exec_command`, an interactive `write_stdin` session,
`apply_patch`, native MCP image propagation from `view_image`, discovery of
the native Browser/Chrome plugin skills, direct `mcp__cua_repl__js`, and generic
execution through that namespaced constructor.
