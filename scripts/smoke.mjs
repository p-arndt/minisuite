// Cross-platform end-to-end smoke runner (Windows + Linux + macOS).
//
//   node scripts/smoke.mjs                # same as `all`
//   node scripts/smoke.mjs minicloak
//   node scripts/smoke.mjs minimail
//   node scripts/smoke.mjs minibucket
//   node scripts/smoke.mjs all            # all three, sequentially
//   node scripts/smoke.mjs all --no-build # skip `cargo build --release`
//
// For each service: build the release binary, start it on a dedicated smoke
// port, wait for that port to accept a TCP connection, run the crate's Python
// smoketest against it, then kill the server — on success, on failure, and on
// Ctrl-C alike. Exits with the Python test's exit code.
//
// This is deliberately a Node script rather than justfile shell: the old
// per-crate `just smoke` recipes used POSIX job control (`&`, `trap`, `$!`),
// which does not exist in PowerShell.
//
// minibucket's smoketest.py hardcodes endpoint http://127.0.0.1:9123 and the
// credentials alice/alicepass, so those values are not configurable here.
// It also needs boto3:  pip install boto3

import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, rmSync } from "node:fs";
import { connect } from "node:net";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const exe = process.platform === "win32" ? ".exe" : "";
const bin = (name) => join(root, "target", "release", name + exe);
const smokeDir = (name) => join(root, "target", "smoke", name);

// Python is `python` on Windows, usually `python3` elsewhere.
const PYTHON = process.env.PYTHON ?? (process.platform === "win32" ? "python" : "python3");

const SERVICES = {
  minicloak: {
    // Port the smoke runner waits for before starting the Python test.
    port: 19500,
    args: () => [
      "--bind",
      "127.0.0.1:19500",
      "--key",
      join(root, "target", "smoke", "minicloak-key.pem"),
    ],
    test: () => [
      join(root, "crates", "minicloak", "smoketest.py"),
      "http://127.0.0.1:19500",
    ],
    // Fresh signing key per run so a stale/corrupt key cannot mask a failure.
    reset: () => rmSync(join(root, "target", "smoke", "minicloak-key.pem"), { force: true }),
  },
  minimail: {
    // The HTTP port comes up after SMTP, so waiting on it covers both.
    port: 18025,
    args: () => [
      "--smtp-bind",
      "127.0.0.1:11025",
      "--http-bind",
      "127.0.0.1:18025",
      "--root",
      smokeDir("minimail"),
    ],
    test: () => [
      join(root, "crates", "minimail", "smoketest.py"),
      "--smtp",
      "127.0.0.1:11025",
      "--http",
      "127.0.0.1:18025",
    ],
    reset: () => {
      rmSync(smokeDir("minimail"), { recursive: true, force: true });
      mkdirSync(smokeDir("minimail"), { recursive: true });
    },
  },
  minibucket: {
    // 9123 + alice/alicepass are hardcoded inside minibucket's smoketest.py.
    port: 9123,
    args: () => [
      "--bind",
      "127.0.0.1:9123",
      "--root",
      smokeDir("minibucket"),
      "--access-key",
      "alice",
      "--secret-key",
      "alicepass",
    ],
    test: () => [join(root, "crates", "minibucket", "smoketest.py")],
    reset: () => {
      rmSync(smokeDir("minibucket"), { recursive: true, force: true });
      mkdirSync(smokeDir("minibucket"), { recursive: true });
    },
  },
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Resolve once the TCP port accepts a connection, or reject after timeoutMs. */
async function waitForPort(port, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const up = await new Promise((resolve) => {
      const sock = connect({ host: "127.0.0.1", port });
      const done = (ok) => {
        sock.destroy();
        resolve(ok);
      };
      sock.setTimeout(1000);
      sock.once("connect", () => done(true));
      sock.once("timeout", () => done(false));
      sock.once("error", () => done(false));
    });
    if (up) return;
    await sleep(200);
  }
  throw new Error(`port ${port} did not open within ${timeoutMs / 1000}s`);
}

/** Run a command to completion, inheriting stdio. Returns the exit code. */
function run(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, {
    cwd: root,
    stdio: "inherit",
    shell: false,
    ...opts,
  });
  if (r.error) {
    console.error(`error: failed to run ${cmd}: ${r.error.message}`);
    return 127;
  }
  return r.status ?? 1;
}

/** Build, launch, wait, test, kill. Returns the exit code of the Python test. */
async function smoke(name, { build }) {
  const svc = SERVICES[name];
  console.log(`\n=== ${name} ===`);

  if (build) {
    const code = run("cargo", ["build", "--release", "-p", name]);
    if (code !== 0) {
      console.error(`${name}: cargo build failed`);
      return code;
    }
  }

  mkdirSync(join(root, "target", "smoke"), { recursive: true });
  svc.reset?.();

  const args = svc.args();
  console.log(`starting: ${bin(name)} ${args.join(" ")}`);
  const server = spawn(bin(name), args, {
    cwd: root,
    stdio: ["ignore", "inherit", "inherit"],
  });

  let serverExit = null;
  server.on("exit", (code, signal) => {
    serverExit = signal ?? code;
  });

  // Kill the server no matter how we leave this function.
  let killed = false;
  const kill = () => {
    if (killed || server.exitCode !== null || serverExit !== null) return;
    killed = true;
    // On Windows there is no SIGTERM: taskkill /T tears down the whole tree.
    if (process.platform === "win32") {
      spawnSync("taskkill", ["/pid", String(server.pid), "/T", "/F"], {
        stdio: "ignore",
      });
    } else {
      server.kill("SIGTERM");
    }
  };
  const onSignal = () => {
    kill();
    process.exit(130);
  };
  process.once("SIGINT", onSignal);
  process.once("SIGTERM", onSignal);

  try {
    if (server.exitCode !== null || serverExit !== null)
      throw new Error(`${name} exited immediately (${serverExit})`);
    await waitForPort(svc.port);
    console.log(`${name} is listening on 127.0.0.1:${svc.port}`);
    const code = run(PYTHON, svc.test());
    if (code !== 0) console.error(`${name}: smoketest failed (exit ${code})`);
    return code;
  } catch (err) {
    console.error(`${name}: ${err.message}`);
    return 1;
  } finally {
    kill();
    process.off("SIGINT", onSignal);
    process.off("SIGTERM", onSignal);
    // Give the OS a moment to release the port before the next service starts.
    await sleep(300);
  }
}

// --- CLI ------------------------------------------------------------------
const argv = process.argv.slice(2);
const build = !argv.includes("--no-build");
const which = argv.find((a) => !a.startsWith("-")) ?? "all";

const names = which === "all" ? Object.keys(SERVICES) : [which];
for (const n of names) {
  if (!SERVICES[n]) {
    console.error(
      `usage: node scripts/smoke.mjs [minicloak|minimail|minibucket|all] [--no-build]`,
    );
    process.exit(1);
  }
}

const results = [];
for (const name of names) results.push([name, await smoke(name, { build })]);

if (results.length > 1) {
  console.log("\n=== summary ===");
  for (const [name, code] of results)
    console.log(`  ${code === 0 ? "PASS" : "FAIL"}  ${name}`);
}

process.exit(results.some(([, code]) => code !== 0) ? 1 : 0);
