import { copyFile, mkdir, readdir, stat, writeFile } from "node:fs/promises";
import { basename, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repoRoot = resolve(fileURLToPath(new URL("..", import.meta.url)));
const bridgeRoot = join(repoRoot, "fkn-bridge");
const releaseRoot = join(bridgeRoot, "target", "release");
const distRoot = join(repoRoot, "dist");

if (process.platform !== "win32") {
  throw new Error("The FKN Windows package can only be assembled on Windows.");
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: repoRoot,
    stdio: "inherit",
    ...options,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`${command} exited with status ${result.status}`);
  }
}

async function existingFile(candidates, description) {
  for (const candidate of candidates.filter(Boolean)) {
    const path = resolve(candidate);
    try {
      if ((await stat(path)).isFile()) return path;
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
  }
  throw new Error(
    `${description} was not found. Checked:\n${candidates
      .filter(Boolean)
      .map((candidate) => `  ${resolve(candidate)}`)
      .join("\n")}`,
  );
}

async function latestInstalledBundle() {
  const programFiles = process.env.ProgramFiles;
  if (!programFiles) return undefined;
  let entries;
  try {
    entries = await readdir(programFiles, { withFileTypes: true });
  } catch (error) {
    if (error.code === "ENOENT" || error.code === "EACCES") return undefined;
    throw error;
  }
  const name = entries
    .filter(
      (entry) =>
        entry.isDirectory() && entry.name.startsWith("fkn-codex-windows-x64-"),
    )
    .map((entry) => entry.name)
    .sort()
    .at(-1);
  return name ? join(programFiles, name) : undefined;
}

function packageLabel() {
  const labelIndex = process.argv.indexOf("--label");
  const explicit = labelIndex >= 0 ? process.argv[labelIndex + 1] : undefined;
  if (labelIndex >= 0 && !explicit) {
    throw new Error("--label requires a value");
  }
  if (explicit && !/^[0-9A-Za-z._-]+$/.test(explicit)) {
    throw new Error(
      "--label may contain only letters, numbers, dots, underscores, and dashes",
    );
  }
  if (explicit) return explicit;
  const now = new Date();
  return [
    now.getFullYear(),
    String(now.getMonth() + 1).padStart(2, "0"),
    String(now.getDate()).padStart(2, "0"),
    "-",
    String(now.getHours()).padStart(2, "0"),
    String(now.getMinutes()).padStart(2, "0"),
    String(now.getSeconds()).padStart(2, "0"),
  ].join("");
}

console.log("Building FKN Codex release binaries...");
run("cargo.exe", [
  "build",
  "--release",
  "--manifest-path",
  join(bridgeRoot, "Cargo.toml"),
  "--bin",
  "fkn-codex",
  "--bin",
  "fkn-codex-bridge",
  "--bin",
  "fkn-codex-auth-shim",
]);

const installedBundle = await latestInstalledBundle();
const codexBin = await existingFile(
  [
    process.env.FKN_PACKAGE_CODEX_BIN,
    join(repoRoot, "codex-rs", "target", "release", "codex.exe"),
    installedBundle && join(installedBundle, "codex.exe"),
  ],
  "codex.exe",
);
const tunnelBin = await existingFile(
  [
    process.env.FKN_PACKAGE_TUNNEL_CLIENT_BIN,
    installedBundle && join(installedBundle, "tunnel-client.exe"),
    process.env.USERPROFILE &&
      join(process.env.USERPROFILE, ".local", "bin", "tunnel-client.exe"),
  ],
  "tunnel-client.exe",
);

const packageName = `fkn-codex-windows-x64-${packageLabel()}`;
const packageDir = join(distRoot, packageName);
const archivePath = join(distRoot, `${packageName}.zip`);
await mkdir(distRoot, { recursive: true });
await mkdir(packageDir, { recursive: false });

const files = [
  [join(releaseRoot, "fkn-codex.exe"), "fkn-codex.exe"],
  [join(releaseRoot, "fkn-codex-bridge.exe"), "fkn-codex-bridge.exe"],
  [join(releaseRoot, "fkn-codex-auth-shim.exe"), "fkn-codex-auth-shim.exe"],
  [codexBin, "codex.exe"],
  [tunnelBin, "tunnel-client.exe"],
];
for (const [source, name] of files) {
  await copyFile(source, join(packageDir, name));
  console.log(`Added ${name} from ${source}`);
}

await writeFile(
  join(packageDir, "README.txt"),
  [
    "FKN Codex for Windows",
    "",
    "1. Extract the entire folder.",
    "2. Open PowerShell in the project you want to use.",
    `3. Run the extracted ${packageName}\\fkn-codex.exe`,
    "",
    "Configuration and the runtime API key are stored separately in %APPDATA%\\FKN Codex.",
    "Do not add config.json or runtime-api-key to this folder before sharing it.",
    "",
  ].join("\r\n"),
  "utf8",
);

console.log(`Creating ${basename(archivePath)}...`);
run(
  "powershell.exe",
  [
    "-NoProfile",
    "-NonInteractive",
    "-Command",
    "Compress-Archive -LiteralPath $env:FKN_PACKAGE_SOURCE -DestinationPath $env:FKN_PACKAGE_ARCHIVE -CompressionLevel Optimal",
  ],
  {
    env: {
      ...process.env,
      FKN_PACKAGE_SOURCE: packageDir,
      FKN_PACKAGE_ARCHIVE: archivePath,
    },
  },
);

console.log(`\nPackage folder: ${packageDir}`);
console.log(`Ready to send:  ${archivePath}`);
