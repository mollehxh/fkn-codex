import { spawn } from "node:child_process";
import { mkdtemp, mkdir, readFile, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const bridgeDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(bridgeDir, "..");
const exe = process.platform === "win32" ? ".exe" : "";
const bridgeBin = process.env.FKN_CODEX_BRIDGE_BIN ?? join(bridgeDir, "target", "debug", `fkn-codex-bridge${exe}`);
const codexBin = process.env.FKN_CODEX_BIN ?? join(repoRoot, "codex-rs", "target", "debug", `codex${exe}`);
const scratch = await mkdtemp(join(tmpdir(), "fkn-direct-tools-smoke-"));
const workspace = join(scratch, "workspace");
const codexHome = join(scratch, "codex-home");
const readyFile = join(scratch, "ready.json");
await mkdir(workspace);
await mkdir(codexHome);

const child = spawn(bridgeBin, [
  "--workspace", workspace,
  "--codex-bin", codexBin,
  "--codex-home", codexHome,
  "--sandbox", "danger-full-access",
  "--disable-cua",
  "--listen", "127.0.0.1:0",
  "--ready-file", readyFile,
], { stdio: ["ignore", "pipe", "pipe"], detached: process.platform !== "win32" });

let exited = false;
let output = "";
child.on("exit", () => { exited = true; });
for (const stream of [child.stdout, child.stderr]) {
  stream.on("data", (chunk) => { output = (output + chunk.toString()).slice(-6000); });
}

const delay = (ms) => new Promise((resolveDelay) => setTimeout(resolveDelay, ms));
async function waitForReadyFile() {
  for (let attempt = 0; attempt < 600; attempt += 1) {
    if (exited) throw new Error(`Bridge exited during startup:\n${output}`);
    try { return JSON.parse(await readFile(readyFile, "utf8")); } catch { await delay(100); }
  }
  throw new Error(`Bridge did not become ready:\n${output}`);
}

let sessionId;
let mcpUrl;
async function rpc(id, method, params) {
  const headers = { "content-type": "application/json", accept: "application/json, text/event-stream" };
  if (sessionId) headers["mcp-session-id"] = sessionId;
  const response = await fetch(mcpUrl, {
    method: "POST", headers,
    body: JSON.stringify({ jsonrpc: "2.0", id, method, params }),
    signal: AbortSignal.timeout(10000),
  });
  if (response.headers.has("mcp-session-id")) sessionId = response.headers.get("mcp-session-id");
  const body = await response.text();
  const messages = body.split("\n").filter((line) => line.startsWith("data: {")).map((line) => JSON.parse(line.slice(6)));
  const message = messages.find((item) => item.id === id);
  if (!response.ok || !message || message.error) throw new Error(`${method} failed: HTTP ${response.status} ${body.slice(-1000)}`);
  return message.result;
}

try {
  mcpUrl = (await waitForReadyFile()).mcp_url;
  const initialized = await rpc(1, "initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "fkn-direct-tools-smoke", version: "1" } });
  if (initialized.capabilities?.tools?.listChanged !== true) throw new Error("Bridge did not advertise dynamic tool-list notifications");
  const cwdStart = initialized.instructions?.indexOf("; cwd=") ?? -1;
  const cwdEnd = initialized.instructions?.indexOf(". Treat cwd", cwdStart) ?? -1;
  if (cwdStart < 0 || cwdEnd < 0) throw new Error(`MCP server instructions did not include the local workspace: ${initialized.instructions}`);
  const advertisedWorkspace = JSON.parse(initialized.instructions.slice(cwdStart + 6, cwdEnd));
  const normalizeWindowsPath = (value) => process.platform === "win32" ? value.replace(/^\\\\\?\\/, "").toLowerCase() : value;
  if (normalizeWindowsPath(advertisedWorkspace) !== normalizeWindowsPath(await realpath(workspace))) throw new Error(`MCP server instructions advertised the wrong workspace: ${initialized.instructions}`);
  if (!initialized.instructions?.includes("access=danger-full-access")) throw new Error(`MCP server instructions did not include the access mode: ${initialized.instructions}`);
  if (Buffer.byteLength(initialized.instructions, "utf8") > 1024) throw new Error("MCP server instructions exceeded the bounded context size");
  const first = await rpc(2, "tools/list", {});
  const skillsList = first.tools?.find((tool) => tool.name === "codex_skills_list");
  if (!skillsList?.description?.includes("MCP server instructions (mirrored")) throw new Error("MCP server context was not mirrored into ChatGPT-visible tool metadata");
  if (!skillsList.description.includes("access=danger-full-access")) throw new Error("Mirrored MCP server context omitted the access mode");
  if (!first.tools?.some((tool) => tool.name === "exec_command")) throw new Error("First Codex turn did not publish exec_command directly");
  if (first.tools?.some((tool) => tool.name.startsWith("mcp__cua_repl__"))) throw new Error("CUA tools were advertised while CUA was disabled");
  const command = await rpc(3, "tools/call", {
    name: "exec_command",
    arguments: {
      cmd: "pwd",
      yield_time_ms: 10000,
      max_output_tokens: 1000,
    },
  });
  if (command.isError || !command.structuredContent?.output?.includes(await realpath(workspace))) {
    throw new Error(`Direct exec_command could not execute pwd: ${JSON.stringify(command)}`);
  }
  console.log("PASS: direct tools execute and optional CUA stays hidden when disabled");
} finally {
  if (!exited) {
    if (process.platform === "win32") {
      const killer = spawn("taskkill.exe", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore" });
      await new Promise((resolveKill, rejectKill) => {
        killer.once("error", rejectKill);
        killer.once("exit", resolveKill);
      });
    } else {
      process.kill(-child.pid, "SIGTERM");
    }
    if (!exited) {
      await new Promise((resolveExit) => child.once("exit", resolveExit));
    }
  }
  await rm(scratch, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
}
