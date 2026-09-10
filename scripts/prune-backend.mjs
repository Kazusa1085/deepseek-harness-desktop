// Removes the parts of `backend/node_modules` that cannot be used at runtime
// from the bundle, so the shipped app does not carry them.
//
// Run from the repository root:  npm run backend:prune
//
//   --check   report what would be removed and exit 1 if anything is left,
//             without deleting it (CI uses this to keep a tree honest)
//
// `npm run backend:install` runs this as its last step, so a tree that was just
// installed is already pruned; `npm ci` restores everything, which is why this
// has to run after every install rather than once.
//
// What goes, and why it is safe:
//
//   *.map        debugger-only source maps; every one has a sibling .js and is
//                referenced solely by a trailing sourceMappingURL comment
//   *.d.ts, *.ts type declarations and package sources; no TypeScript compiler
//                or runner is shipped, and nothing resolves an entry point into
//                them (`exports` maps point at the built .js)
//   *.md         package documentation
//   *.pdb        Windows debug symbols for node-pty's ConPTY helper (~20 MB)
//   foreign-platform binaries: node-pty prebuilds, @vscode/ripgrep-<os>-<arch>,
//                @koromix/koffi-<os>-<arch>, @img/sharp-* for an OS this bundle
//                cannot run on. Architectures of the target OS are all kept, so
//                a universal macOS bundle keeps both darwin prebuilds.
//
// Anything else — including test directories, fixtures and licenses — is left
// alone on purpose: the audit that produced this list only verified the
// categories above. Set `DSH_BUNDLE_OSES` (comma-separated, e.g.
// `darwin,linux`) when cross-building for another OS, otherwise the target is
// this machine's.
import { readdirSync, rmSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, extname, join, relative } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const modules = join(root, "backend/node_modules");
const checkOnly = process.argv.includes("--check");

const bundleOses = (process.env.DSH_BUNDLE_OSES ?? process.platform)
  .split(",")
  .map((os) => os.trim())
  .filter((os) => os.length > 0);

const OS_TOKENS = ["win32", "darwin", "linux", "freebsd", "android"];
const PLATFORM_PACKAGE_PREFIXES = ["@vscode/ripgrep-", "@koromix/koffi-", "@img/sharp-"];

/** Whether one file is dead weight for a bundle targeting `bundleOses`. */
function isPrunable(relativePath) {
  const name = relativePath.split("/").pop() ?? "";
  if (/\.d\.(ts|mts|cts)$/.test(name)) {
    return "type declarations";
  }
  const extension = extname(name);
  if (extension === ".pdb") {
    return "Windows debug symbols";
  }
  if (extension === ".map") {
    return "source maps";
  }
  if (extension === ".md") {
    return "package documentation";
  }
  if (extension === ".ts" || extension === ".mts" || extension === ".cts") {
    return "package sources";
  }

  const parts = relativePath.split("/");
  // node-pty/<prebuilds>/<os>-<arch>/…, and platform-specific packages such as
  // @vscode/ripgrep-linux-x64/… at the top level.
  const prebuild = parts.indexOf("prebuilds");
  if (prebuild >= 0 && parts.length > prebuild + 1) {
    const platform = parts[prebuild + 1];
    const os = OS_TOKENS.find((token) => platform.startsWith(`${token}-`));
    if (os !== undefined && !bundleOses.includes(os)) {
      return "another platform's binaries";
    }
  }
  for (const prefix of PLATFORM_PACKAGE_PREFIXES) {
    if (!relativePath.startsWith(prefix)) {
      continue;
    }
    const rest = relativePath.slice(prefix.length);
    const os = OS_TOKENS.find((token) => rest.startsWith(`${token}-`));
    if (os !== undefined && !bundleOses.includes(os)) {
      return "another platform's binaries";
    }
  }
  return undefined;
}

/** Every regular file under `dir`, as paths relative to it. */
function* walk(dir, base = dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      yield* walk(full, base);
    } else if (entry.isFile()) {
      yield relative(base, full);
    }
  }
}

function megabytes(bytes) {
  return `${(bytes / 1048576).toFixed(1)} MB`;
}

if (statSync(modules, { throwIfNoEntry: false }) === undefined) {
  console.log("[dsh-desktop] backend/node_modules is absent; nothing to prune");
  process.exit(0);
}

const reasons = new Map();
let files = 0;
let bytes = 0;
const doomed = [];

for (const path of walk(modules)) {
  const reason = isPrunable(path);
  if (reason === undefined) {
    continue;
  }
  const size = statSync(join(modules, path)).size;
  doomed.push([path, size]);
  reasons.set(reason, (reasons.get(reason) ?? { files: 0, bytes: 0 }));
  const bucket = reasons.get(reason);
  bucket.files += 1;
  bucket.bytes += size;
  files += 1;
  bytes += size;
}

if (files === 0) {
  console.log(`[dsh-desktop] runtime already pruned (target: ${bundleOses.join(", ")})`);
  process.exit(0);
}

for (const [reason, bucket] of reasons) {
  console.log(`[dsh-desktop]   ${reason}: ${bucket.files} files, ${megabytes(bucket.bytes)}`);
}

if (checkOnly) {
  console.error(
    `[dsh-desktop] ${files} prunable files (${megabytes(bytes)}) remain; run \`npm run backend:prune\``,
  );
  process.exit(1);
}

for (const [path] of doomed) {
  rmSync(join(modules, path), { force: true });
}
console.log(
  `[dsh-desktop] pruned ${files} files (${megabytes(bytes)}) from backend/node_modules for ${bundleOses.join(", ")}`,
);
