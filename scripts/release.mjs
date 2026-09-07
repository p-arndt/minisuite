// Cut a release of the whole minisuite workspace.
//
//   node scripts/release.mjs              # patch bump (default)
//   node scripts/release.mjs minor
//   node scripts/release.mjs major
//   node scripts/release.mjs 1.4.0        # explicit version
//   node scripts/release.mjs minor --dry-run   # print the plan, write nothing
//   node scripts/release.mjs minor --no-push   # commit + tag locally only
//
// Why a wrapper instead of plain `stamp release`?
//
// stamp owns the version (see .stamp.yml: Cargo.toml#workspace.package.version)
// but it cannot rewrite Cargo.lock — the version lives there once per workspace
// member, inside `[[package]]` array-of-tables entries, and stamp refuses to
// guess which entry a key path means. Cargo does that job perfectly well, so
// the flow is:
//
//   1. stamp set <bump>            -> Cargo.toml only, no git
//   2. cargo update --workspace    -> Cargo.lock follows
//   3. git commit                  -> both files, in one "release: v<x.y.z>" commit
//   4. stamp release <ver> -y      -> checks, tags HEAD, pushes branch + tag
//
// Step 4 asks for the version that is already committed. That is stamp's
// first-release case: "Equal is allowed: stamp then writes nothing and tags
// HEAD" — so the preflight still runs in full, but the commit is ours.
//
// Requires `stamp` on PATH:
//   irm https://raw.githubusercontent.com/p-arndt/stamp/main/install.ps1 | iex   (Windows)
//   curl -fsSL https://raw.githubusercontent.com/p-arndt/stamp/main/install.sh | sh

import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

function fail(msg) {
  console.error(`error: ${msg}`);
  process.exit(1);
}

/** Run a command, streaming its output. Exits the script if it fails. */
function run(cmd, args) {
  console.log(`\n$ ${cmd} ${args.join(" ")}`);
  const r = spawnSync(cmd, args, { cwd: root, stdio: "inherit", shell: false });
  if (r.error)
    fail(
      r.error.code === "ENOENT"
        ? `${cmd} is not on PATH` +
            (cmd === "stamp"
              ? " — install it: https://github.com/p-arndt/stamp#-install"
              : "")
        : `failed to run ${cmd}: ${r.error.message}`,
    );
  if (r.status !== 0) fail(`${cmd} ${args[0]} exited with ${r.status}`);
}

/** Capture a command's stdout. Returns null if it could not run. */
function capture(cmd, args) {
  const r = spawnSync(cmd, args, { cwd: root, encoding: "utf8", shell: false });
  if (r.error || r.status !== 0) return null;
  return r.stdout.trim();
}

/**
 * Read the [workspace.package] version out of root Cargo.toml.
 *
 * Scoped to that table on purpose: a bare /^version = "..."/m would also match
 * a `version` key in another table.
 */
function readVersion() {
  const toml = readFileSync(join(root, "Cargo.toml"), "utf8");
  const header = /^\[workspace\.package\][ \t]*\r?$/m.exec(toml);
  if (!header) fail("no [workspace.package] table in Cargo.toml");
  const bodyStart = header.index + header[0].length;
  const rest = toml.slice(bodyStart);
  const next = /^\[[^\]]+\]/m.exec(rest);
  const body = rest.slice(0, next ? next.index : rest.length);
  const m = /^version[ \t]*=[ \t]*"([^"]*)"/m.exec(body);
  if (!m) fail("no version key in [workspace.package]");
  return m[1];
}

// --- CLI ------------------------------------------------------------------
const argv = process.argv.slice(2);
const dryRun = argv.includes("--dry-run");
const noPush = argv.includes("--no-push");
const bump = argv.find((a) => !a.startsWith("-")) ?? "patch";

// A dry run must not touch anything, so hand it straight to stamp: its plan and
// preflight are exactly what the real release would do to Cargo.toml and git.
// (The `cargo update` step it does not show is mechanical and cannot fail a
// release on its own.)
if (dryRun) {
  console.log(
    `Dry run: stamp's plan for a "${bump}" release. Nothing is written; note that\n` +
      "the real run additionally refreshes Cargo.lock via `cargo update --workspace`.",
  );
  run("stamp", ["release", bump, "--dry-run"]);
  process.exit(0);
}

// The release commit must hold nothing but the version bump. stamp checks this
// too, but only in step 4 — by then we would already have edited two files.
const status = capture("git", ["status", "--porcelain"]);
if (status === null) fail("git is not available, or this is not a repository");
if (status) fail("working tree is not clean — commit or stash your changes first.");

const before = readVersion();

// 1. Version files (Cargo.toml). No git, so nothing is committed yet.
run("stamp", ["set", bump]);

const next = readVersion();
console.log(`\nReleasing v${next}  (was ${before})`);

// 2. Cargo.lock follows the new workspace version. Not --offline: the lockfile
//    must end up exactly as CI's `--locked` build expects it.
run("cargo", ["update", "--workspace"]);

// 3. One commit holding both files. `git add` is explicit rather than `-A` so a
//    stray file appearing mid-release cannot slip into the release commit.
run("git", ["add", "Cargo.toml", "Cargo.lock"]);
run("git", ["commit", "-m", `release: v${next}`]);

// 4. stamp checks, tags HEAD and pushes branch + tag together. The version it
//    is given equals the committed one, so it writes nothing.
run("stamp", ["release", next, "-y", ...(noPush ? ["--no-push"] : [])]);

console.log(
  noPush
    ? `\nDone locally. Push branch + tag to trigger the release workflow.`
    : `\nDone. v${next} pushed — the "Release" workflow is now running.`,
);
