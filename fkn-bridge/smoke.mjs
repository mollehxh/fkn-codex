import { spawn } from "node:child_process";
import { mkdtemp, mkdir, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const CALL_ID = "fkn-smoke-pwd";
const bridgeDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(bridgeDir, "..");
const codexRs = join(repoRoot, "codex-rs");

function sendSse(response, events) {
  response.writeHead(200, {
    "cache-control": "no-cache",
    "content-type": "text/event-stream",
  });

  for (const event of events) {
    response.write(`event: ${event.type}\n`);
    response.write(`data: ${JSON.stringify(event)}\n\n`);
  }

  response.end();
}

function completed(id) {
  return {
    type: "response.completed",
    response: {
      id,
      usage: {
        input_tokens: 0,
        input_tokens_details: null,
        output_tokens: 0,
        output_tokens_details: null,
        total_tokens: 0,
      },
    },
  };
}

function findFunctionOutput(value) {
  if (Array.isArray(value)) {
    for (const item of value) {
      const found = findFunctionOutput(item);
      if (found !== undefined) return found;
    }
    return undefined;
  }

  if (!value || typeof value !== "object") return undefined;

  if (value.type === "function_call_output" && value.call_id === CALL_ID) {
    return value.output;
  }

  for (const child of Object.values(value)) {
    const found = findFunctionOutput(child);
    if (found !== undefined) return found;
  }

  return undefined;
}

function advertisedToolNames(body) {
  if (!Array.isArray(body.tools)) return [];
  return body.tools
    .map((tool) => tool?.name ?? tool?.function?.name)
    .filter((name) => typeof name === "string");
}

async function readJson(request) {
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}

const state = {
  requestCount: 0,
  toolOutput: undefined,
};

const server = createServer(async (request, response) => {
  try {
    if (request.method !== "POST" || request.url !== "/v1/responses") {
      response.writeHead(404).end();
      return;
    }

    const body = await readJson(request);
    state.requestCount += 1;

    if (state.requestCount === 1) {
      const tools = advertisedToolNames(body);
      if (!tools.includes("exec_command")) {
        throw new Error(
          `Codex did not advertise exec_command. Tools: ${tools.join(", ")}`,
        );
      }

      console.log(`[bridge] Codex advertised ${tools.length} tools`);
      console.log("[bridge] asking native Codex runtime to execute: pwd");

      sendSse(response, [
        {
          type: "response.created",
          response: { id: "fkn-smoke-response-1" },
        },
        {
          type: "response.output_item.done",
          item: {
            type: "function_call",
            call_id: CALL_ID,
            name: "exec_command",
            arguments: JSON.stringify({
              cmd: "pwd",
              yield_time_ms: 10_000,
              max_output_tokens: 2_000,
            }),
          },
        },
        completed("fkn-smoke-response-1"),
      ]);
      return;
    }

    const output = findFunctionOutput(body);
    if (output === undefined) {
      throw new Error("Expected Codex to return function_call_output for pwd");
    }

    state.toolOutput = output;
    console.log("[bridge] received native Codex tool result:");
    console.log(typeof output === "string" ? output : JSON.stringify(output, null, 2));

    sendSse(response, [
      {
        type: "response.output_item.done",
        item: {
          type: "message",
          role: "assistant",
          id: "fkn-smoke-message-1",
          content: [
            {
              type: "output_text",
              text: "FKN bridge smoke complete",
            },
          ],
        },
      },
      completed("fkn-smoke-response-2"),
    ]);
  } catch (error) {
    console.error("[bridge] request failed", error);
    if (!response.headersSent) {
      response.writeHead(500, { "content-type": "application/json" });
    }
    response.end(JSON.stringify({ error: String(error) }));
  }
});

await new Promise((resolveListen, rejectListen) => {
  server.once("error", rejectListen);
  server.listen(0, "127.0.0.1", resolveListen);
});

const address = server.address();
if (!address || typeof address === "string") {
  throw new Error("Failed to resolve bridge listen address");
}

const scratchRoot = await mkdtemp(join(tmpdir(), "fkn-codex-smoke-"));
const codexHome = join(scratchRoot, "codex-home");
const workspace = join(scratchRoot, "workspace");
await mkdir(codexHome, { recursive: true });
await mkdir(workspace, { recursive: true });

const provider = `model_providers.fkn={ name = "fkn-bridge", base_url = "http://127.0.0.1:${address.port}/v1", env_key = "FKN_CODEX_BRIDGE_KEY", wire_api = "responses" }`;

console.log(`[bridge] listening on http://127.0.0.1:${address.port}/v1/responses`);
console.log(`[bridge] isolated CODEX_HOME: ${codexHome}`);
console.log(`[bridge] smoke workspace: ${workspace}`);

const child = spawn(
  "cargo",
  [
    "run",
    "-q",
    "-p",
    "codex-cli",
    "--bin",
    "codex",
    "--",
    "exec",
    "--skip-git-repo-check",
    "--model",
    "gpt-5.4",
    "-c",
    provider,
    "-c",
    'model_provider="fkn"',
    "-C",
    workspace,
    "Run the bridge smoke test.",
  ],
  {
    cwd: codexRs,
    env: {
      ...process.env,
      CODEX_HOME: codexHome,
      FKN_CODEX_BRIDGE_KEY: "dummy",
    },
    stdio: ["ignore", "pipe", "pipe"],
  },
);

child.stdout.on("data", (chunk) => process.stdout.write(`[codex] ${chunk}`));
child.stderr.on("data", (chunk) => process.stderr.write(`[codex] ${chunk}`));

const exitCode = await new Promise((resolveExit, rejectExit) => {
  child.once("error", rejectExit);
  child.once("exit", (code) => resolveExit(code));
});

server.close();
await rm(scratchRoot, { recursive: true, force: true });

if (exitCode !== 0) {
  throw new Error(`Codex exited with code ${exitCode}`);
}

if (state.requestCount < 2 || state.toolOutput === undefined) {
  throw new Error("Smoke flow did not complete the tool-call round trip");
}

console.log("\nPASS: local Responses bridge -> native Codex exec_command -> bridge result");
