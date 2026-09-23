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
const scratch = await mkdtemp(join(tmpdir(), "fkn-lifecycle-smoke-"));
const workspace = join(scratch, "workspace");
const codexHome = join(scratch, "codex-home");
const readyFile = join(scratch, "ready.json");
await mkdir(workspace);
await mkdir(codexHome);

const child = spawn(bridgeBin, [
  "--workspace", workspace,
  "--codex-bin", codexBin,
  "--codex-home", codexHome,
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
  await rpc(1, "initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "fkn-lifecycle-smoke", version: "1" } });
  const first = await rpc(2, "tools/call", { name: "codex_inventory", arguments: {} });
  if (first.structuredContent?.ready !== true) throw new Error("First Codex turn was not ready");
  await rpc(3, "tools/call", { name: "codex_finish", arguments: { message: "First turn complete" } });

  let secondReady = false;
  for (let attempt = 0; attempt < 150; attempt += 1) {
    if (exited) throw new Error(`Bridge exited after codex_finish:\n${output}`);
    try {
      const next = await rpc(4 + attempt, "tools/call", { name: "codex_inventory", arguments: {} });
      if (next.structuredContent?.ready === true) { secondReady = true; break; }
    } catch (error) {
      if (exited) throw error;
    }
    await delay(100);
  }
  if (!secondReady) throw new Error(`Second Codex turn was not ready:\n${output}`);
  const command = await rpc(200, "tools/call", {
    name: "codex_exec",
    arguments: {
      call_type: "function",
      name: "exec_command",
      arguments: { cmd: "pwd", yield_time_ms: 10000, max_output_tokens: 1000 },
    },
  });
  if (command.isError || !command.structuredContent?.output?.includes(await realpath(workspace))) {
    throw new Error(`Second Codex turn could not execute pwd: ${JSON.stringify(command)}`);
  }
  console.log("PASS: bridge remains available after codex_finish and executes in a second turn");
} finally {
  if (!exited) {
    if (process.platform === "win32") child.kill();
    else process.kill(-child.pid, "SIGTERM");
  }
  await rm(scratch, { recursive: true, force: true });
}
