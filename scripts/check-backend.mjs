// Smoke-tests the installed harness runtime: starts it exactly the way the
// desktop shell does and waits for it to announce its loopback URL.
//
//   node scripts/check-backend.mjs
//
// This exists because a tree can install cleanly and still be unusable. A
// missing peer dependency shipped in v0.1.3 and only surfaced as
// ERR_MODULE_NOT_FOUND on a user's machine — `npm install` reported success
// throughout. Running the thing is the only check that means anything.
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

// Run under the interpreter the portable build embeds, when it is staged.
// Checking with whatever Node happens to be on the runner tests the wrong
// thing: CI defaulted to Node 20, which lacks the zstd support in `node:zlib`
// that the harness needs, so the check failed on a perfectly good tree.
const vendored = join(root, "src-tauri/vendor", process.platform === "win32" ? "node.exe" : "node");
const interpreter = existsSync(vendored) ? vendored : process.execPath;
const entry = join(
  root,
  "backend/node_modules/@deepseek-ai/dsh/lib/bin.js".replace(/\//g, "/"),
);
const TIMEOUT_MS = 120_000;

if (!existsSync(entry)) {
  console.error(`[check-backend] not installed: ${entry}`);
  console.error("[check-backend] run `npm run backend:install` first");
  process.exit(1);
}

console.log(`[check-backend] using ${interpreter}`);
const child = spawn(interpreter, [entry, "web", "--no-open", "--port", "0"], {
  stdio: ["ignore", "pipe", "pipe"],
});

const output = [];
let settled = false;

function finish(code, message) {
  if (settled) return;
  settled = true;
  clearTimeout(timer);
  child.kill("SIGKILL");
  if (code === 0) {
    console.log(`[check-backend] ${message}`);
  } else {
    console.error(`[check-backend] ${message}`);
    console.error(output.join("").trimEnd() || "(the backend produced no output)");
  }
  process.exit(code);
}

const timer = setTimeout(
  () => finish(1, `no URL announced within ${TIMEOUT_MS / 1000}s`),
  TIMEOUT_MS,
);

// Readiness is not "a port number appeared": the harness mints a per-run
// `?token=` and answers the bare origin with `401 dsh web authentication
// required`. The printed URL is therefore the only thing worth checking, and it
// has to be used exactly as printed — which is also what the Rust shell does.
async function verifyReady(url) {
  try {
    const response = await fetch(url, { redirect: "manual" });
    const cookie = response.headers.get("set-cookie") ?? "";
    if (response.status === 303 && /dsh-auth-/.test(cookie)) {
      finish(0, `backend ready at ${new URL(url).origin} (token handshake ok)`);
    } else if (response.status === 200) {
      // Older harness builds announce a token-free URL and serve it directly.
      finish(0, `backend ready at ${new URL(url).origin} (no token required)`);
    } else {
      finish(1, `the announced URL did not authenticate the browser session (HTTP ${response.status})`);
    }
  } catch (error) {
    finish(1, `could not reach the announced URL: ${error.message}`);
  }
}

for (const stream of [child.stdout, child.stderr]) {
  stream.setEncoding("utf8");
  stream.on("data", (chunk) => {
    output.push(chunk);
    const match = /dsh web: (https?:\/\/\S+)/.exec(output.join(""));
    if (match !== null && !settled) verifyReady(match[1]);
  });
}

child.on("error", (error) => finish(1, `could not start the backend: ${error.message}`));
child.on("exit", (code) => finish(1, `backend exited with code ${code} before announcing a URL`));
