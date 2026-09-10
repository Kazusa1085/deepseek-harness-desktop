// Installs the DeepSeek Harness runtime into `backend/node_modules` so the
// Rust shell can find `backend/node_modules/@deepseek-ai/dsh/lib/bin.js`.
//
// Run from the repository root:  npm run backend:install
//
// `node scripts/install-backend.mjs --check-artifacts` skips the install and
// only runs the native-piece assertion below, against whatever tree is already
// in place.
//
// `backend/package-lock.json` is committed, and `npm ci` is used whenever it is
// present. That is not just about speed. Resolving this dependency graph from
// scratch is genuinely expensive — npm ran the GitHub macOS runner out of heap
// doing it — and the obvious workaround, `--legacy-peer-deps`, silently drops
// peer dependencies the harness actually needs, producing a tree that installs
// cleanly and dies at runtime. A lockfile avoids both: no resolution, and the
// exact set of packages that was tested.
//
// Without a lockfile it falls back to pnpm, then bun, then npm. pnpm is given
// the hoisted linker because its default symlink layout cannot be packed into
// the portable build.
import { spawnSync } from "node:child_process";
import { existsSync, readdirSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const backend = join(root, "backend");
const windows = process.platform === "win32";

function has(cmd) {
  const probe = spawnSync(cmd, ["--version"], { stdio: "ignore", shell: windows });
  return probe.error === undefined && probe.status === 0;
}

function resolveInstaller() {
  if (existsSync(join(backend, "package-lock.json"))) {
    return { command: "npm", args: ["ci", "--no-audit", "--no-fund"] };
  }
  if (has("pnpm")) {
    return { command: "pnpm", args: ["install", "--no-frozen-lockfile", "--node-linker=hoisted"] };
  }
  if (has("bun")) {
    return { command: "bun", args: ["install"] };
  }
  return { command: "npm", args: ["install", "--no-audit", "--no-fund"] };
}

/** Every file named `name` under `dir`, searched to a bounded depth. */
function findNamed(dir, name, depth = 3) {
  if (depth < 0 || !existsSync(dir)) {
    return undefined;
  }
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isFile() && entry.name === name) {
      return full;
    }
    if (entry.isDirectory()) {
      const found = findNamed(full, name, depth - 1);
      if (found !== undefined) {
        return found;
      }
    }
  }
  return undefined;
}

/**
 * Assert the native pieces that only exist once install scripts have run.
 *
 * npm 12 runs lifecycle scripts only for packages named in `allowScripts`, and
 * those entries carry EXACT versions. Bumping the harness therefore stops
 * matching them, the scripts are skipped, and the damage surfaces much later as
 * a PTY or a file-search failure inside a session. Checking the artifacts here
 * turns that silent breakage into a failed install, with the fix in the message.
 */
function assertNativePieces() {
  const modules = join(backend, "node_modules");
  const platform = process.platform;
  const arch = process.arch === "arm64" ? "arm64" : "x64";
  const problems = [];

  const check = (found, label, { executable = false } = {}) => {
    if (found === undefined) {
      problems.push(`${label} is missing`);
      return;
    }
    if (executable && !windows && (statSync(found).mode & 0o111) === 0) {
      problems.push(`${label} is not executable (${found})`);
    }
  };

  // node-pty ships prebuilds per platform; only macOS carries the spawn helper,
  // and it must stay executable or `pty.fork` fails at the first shell.
  const ptyDir = join(modules, "node-pty", "prebuilds", `${platform}-${arch}`);
  if (existsSync(ptyDir)) {
    check(findNamed(ptyDir, "pty.node", 1), "node-pty prebuild (pty.node)");
    if (platform === "darwin") {
      check(findNamed(ptyDir, "spawn-helper", 1), "node-pty spawn-helper", { executable: true });
    }
  }

  // File search resolves ripgrep out of its platform package with no PATH
  // fallback, so a stripped executable bit breaks every search.
  check(
    findNamed(join(modules, `@vscode/ripgrep-${platform}-${arch}`), windows ? "rg.exe" : "rg"),
    "ripgrep binary (@vscode/ripgrep)",
    { executable: !windows },
  );

  // koffi's prebuilt binary arrives in its platform package, not from a build.
  check(
    findNamed(join(modules, `@koromix/koffi-${platform}-${arch}`), "koffi.node"),
    "koffi native module",
  );

  if (problems.length === 0) {
    console.log("[dsh-desktop] native pieces present (node-pty, ripgrep, koffi)");
    return;
  }
  console.error("[dsh-desktop] the runtime installed but is not usable:");
  for (const problem of problems) {
    console.error(`[dsh-desktop]   - ${problem}`);
  }
  console.error(
    "[dsh-desktop] these come from install scripts, which npm 12 only runs for packages named in " +
      "backend/package.json's allowScripts. After bumping @deepseek-ai/dsh, update those keys to the " +
      "new versions (backend/package-lock.json lists every package with hasInstallScript) and reinstall.",
  );
  process.exit(1);
}

const { command, args } = resolveInstaller();

if (process.argv.includes("--check-artifacts")) {
  console.log(`[dsh-desktop] checking the installed runtime in ${backend}`);
  assertNativePieces();
  process.exit(0);
}

console.log(`[dsh-desktop] installing backend runtime with ${command} ${args[0]} in ${backend}`);

const result = spawnSync(command, args, {
  cwd: backend,
  stdio: "inherit",
  shell: windows,
  // Resolution is memory-hungry on this graph; harmless for `npm ci`.
  env: { ...process.env, NODE_OPTIONS: `${process.env.NODE_OPTIONS ?? ""} --max-old-space-size=6144`.trim() },
});

if (result.error) {
  console.error(`[dsh-desktop] failed to run ${command}: ${result.error.message}`);
  process.exit(1);
}
if (result.status !== 0) {
  console.error(
    `[dsh-desktop] ${command} ${args[0]} exited with ${
      result.status === null ? "no status (killed by a signal)" : `status ${result.status}`
    }`,
  );
  process.exit(result.status ?? 1);
}

assertNativePieces();

// Last step, and it belongs here rather than in a one-off command: `npm ci`
// restores every file the prune removes, so a tree pruned yesterday is a full
// 296 MB tree again today. CI asserts the invariant with `--check`.
const prune = spawnSync(process.execPath, [join(root, "scripts/prune-backend.mjs")], {
  stdio: "inherit",
});
if (prune.error !== undefined) {
  console.error(`[dsh-desktop] could not prune the runtime: ${prune.error.message}`);
  process.exit(1);
}
process.exit(prune.status ?? 1);
