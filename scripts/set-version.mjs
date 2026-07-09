// Stamp / bump the minicloak version.
//
//   node scripts/set-version.mjs 0.2.0     # set an explicit version
//   node scripts/set-version.mjs patch     # bump 0.1.3 -> 0.1.4
//   node scripts/set-version.mjs minor     # bump 0.1.3 -> 0.2.0
//   node scripts/set-version.mjs major     # bump 0.1.3 -> 1.0.0
//
// minicloak keeps its version in ONE place: the `version` key of [package] in
// Cargo.toml. The binary reads it back through env!("CARGO_PKG_VERSION") for its
// `--version` output and its Server header, so nothing else needs stamping.
//
// The rewrite is a targeted regex, not a TOML round-trip, so existing formatting,
// key order and comments survive untouched.
//
// Also exports readVersion / bumpVersion / setVersion / resolveVersion for
// scripts/release.mjs.

import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

// Repo root is one level up from this script's scripts/ directory.
const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const cargoToml = join(root, "Cargo.toml");

// The (?m)^ anchor hits the [package] `version = "..."` line. minicloak has no
// dependencies and no `rust-version` key, so this is the only such line in the
// file; setVersion() additionally refuses to write when the pattern goes stale.
const VERSION_RE = /^(version = )"[^"]*"/m;

/** Read the current version from Cargo.toml. */
export function readVersion() {
  const m = VERSION_RE.exec(readFileSync(cargoToml, "utf8"));
  if (!m) throw new Error("could not find the package version in Cargo.toml");
  return m[0].match(/"([^"]*)"/)[1];
}

/** Bump a semver string by "patch" | "minor" | "major". */
export function bumpVersion(current, kind) {
  const m = /^(\d+)\.(\d+)\.(\d+)$/.exec(current);
  if (!m) throw new Error(`current version is not plain semver: ${current}`);
  let [major, minor, patch] = m.slice(1).map(Number);
  if (kind === "major") [major, minor, patch] = [major + 1, 0, 0];
  else if (kind === "minor") [minor, patch] = [minor + 1, 0];
  else if (kind === "patch") patch++;
  else throw new Error(`unknown bump "${kind}" (use patch|minor|major)`);
  return `${major}.${minor}.${patch}`;
}

/** Write `version` into Cargo.toml. Throws if the pattern is stale. */
export function setVersion(version) {
  if (!/^\d+\.\d+\.\d+/.test(version))
    throw new Error(`invalid version "${version}" (expected x.y.z)`);
  const before = readFileSync(cargoToml, "utf8");
  const after = before.replace(VERSION_RE, `$1"${version}"`);
  if (after === before)
    throw new Error("no version match in Cargo.toml — pattern may be stale");
  writeFileSync(cargoToml, after);
  console.log(`  Cargo.toml`);
  console.log(`Stamped version ${version}.`);
}

// Resolve a CLI argument to a concrete version: a bump keyword or an explicit x.y.z.
export function resolveVersion(arg) {
  return ["patch", "minor", "major"].includes(arg)
    ? bumpVersion(readVersion(), arg)
    : arg;
}

// CLI entry point (only when run directly, not when imported).
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const arg = process.argv[2];
  if (!arg) {
    console.error("usage: node scripts/set-version.mjs <patch|minor|major|x.y.z>");
    process.exit(1);
  }
  setVersion(resolveVersion(arg));
}
