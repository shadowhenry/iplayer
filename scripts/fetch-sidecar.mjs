#!/usr/bin/env node
/**
 * Downloads self-contained (static) ffmpeg / ffprobe sidecars into
 * `src-tauri/binaries/` using the Tauri sidecar naming convention
 * (`<name>-<target-triple>`).
 *
 * On macOS we always fetch BOTH x86_64 and aarch64 static builds, so the
 * universal (`--target universal-apple-darwin`) build has a sidecar for each
 * architecture — without this, `tauri build --target universal-apple-darwin`
 * fails with "resource path `binaries/ffmpeg-x86_64-apple-darwin` doesn't exist".
 * The binaries come from the `eugeneware/ffmpeg-static` GitHub release, which
 * bundles ffmpeg AND ffprobe for every platform as fully static executables
 * (no Homebrew dylib dependencies), so they are safe to ship inside a dmg.
 *
 * On other platforms we resolve the host-arch binary from the
 * `ffmpeg-static` / `ffprobe-static` npm packages, or the system PATH.
 *
 * Usage:  node scripts/fetch-sidecar.mjs
 */
import { chmodSync, copyFileSync, existsSync, mkdirSync, statSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const outDir = join(root, "src-tauri", "binaries");
mkdirSync(outDir, { recursive: true });

const TRIPLE = {
  "darwin arm64": "aarch64-apple-darwin",
  "darwin x64": "x86_64-apple-darwin",
  "linux x64": "x86_64-unknown-linux-gnu",
  "linux arm64": "aarch64-unknown-linux-gnu",
  "win32 x64": "x86_64-pc-windows-msvc",
  "win32 arm64": "aarch64-pc-windows-msvc",
}[`${process.platform} ${process.arch}`];

// Map a ffmpeg-static GitHub asset name to the Tauri sidecar filename.
// Bump FFMPEG_STATIC_TAG if a newer static build is needed.
const FFMPEG_STATIC_TAG = process.env.FFMPEG_STATIC_TAG || "b6.1.1";
const DARWIN_ASSETS = [
  { asset: "ffmpeg-darwin-x64", name: "ffmpeg-x86_64-apple-darwin" },
  { asset: "ffmpeg-darwin-arm64", name: "ffmpeg-aarch64-apple-darwin" },
  { asset: "ffprobe-darwin-x64", name: "ffprobe-x86_64-apple-darwin" },
  { asset: "ffprobe-darwin-arm64", name: "ffprobe-aarch64-apple-darwin" },
];

async function download(url, dest) {
  const res = await fetch(url, { headers: { "User-Agent": "iplayer-sidecar" } });
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
  const buf = Buffer.from(await res.arrayBuffer());
  writeFileSync(dest, buf);
}

async function fetchDarwin() {
  const base = `https://github.com/eugeneware/ffmpeg-static/releases/download/${FFMPEG_STATIC_TAG}`;
  let ok = 0;
  for (const { asset, name } of DARWIN_ASSETS) {
    const dest = join(outDir, name);
    try {
      await download(`${base}/${asset}`, dest);
      chmodSync(dest, 0o755);
      const mb = (statSync(dest).size / 1024 / 1024).toFixed(1);
      console.log(`  ✓ ${name}  ${mb} MB  (${FFMPEG_STATIC_TAG})`);
      ok++;
    } catch (e) {
      console.error(`  ✗ ${name}: ${e.message}`);
    }
  }
  return ok === DARWIN_ASSETS.length;
}

function fromPackage(pkg) {
  try {
    const p = require(pkg);
    if (typeof p === "string" && existsSync(p)) return p;
    if (p && typeof p.path === "string" && existsSync(p.path)) return p.path;
  } catch {
    /* not installed */
  }
  return null;
}

function fromPath(name) {
  try {
    const out = require("node:child_process")
      .execFileSync(process.platform === "win32" ? "where" : "which", [name], { encoding: "utf8" })
      .trim();
    const first = out.split(/\r?\n/)[0];
    return first && existsSync(first) ? first : null;
  } catch {
    return null;
  }
}

async function main() {
  if (process.platform === "darwin") {
    console.log("Fetching universal sidecars (x86_64 + aarch64) for macOS…");
    const ok = await fetchDarwin();
    if (ok) {
      console.log(`\nSidecars ready for universal-apple-darwin in src-tauri/binaries/`);
      return;
    }
    console.error("\nGitHub fetch incomplete — falling back to host-arch only.");
  }

  // Non-darwin, or darwin fallback: host arch only.
  if (!TRIPLE) {
    console.error(`Unsupported platform: ${process.platform} ${process.arch}`);
    process.exit(1);
  }
  const jobs = [
    { name: "ffmpeg", source: fromPackage("ffmpeg-static") || fromPath("ffmpeg") },
    { name: "ffprobe", source: fromPackage("ffprobe-static") || fromPath("ffprobe") },
  ];
  let failed = false;
  for (const { name, source } of jobs) {
    if (!source) {
      console.error(`  ✗ ${name}: not found (npm i ffmpeg-static ffprobe-static, or install ${name})`);
      failed = true;
      continue;
    }
    const dest = join(outDir, `${name}-${TRIPLE}`);
    copyFileSync(source, dest);
    chmodSync(dest, 0o755);
    const size = (statSync(dest).size / 1024 / 1024).toFixed(1);
    console.log(`  ✓ ${name}  ${size} MB  ← ${source}`);
  }
  if (failed) {
    console.error("\nSome sidecars are missing — iPlayer will fall back to a system ffmpeg at runtime.");
    process.exitCode = 0;
  } else {
    console.log(`\nSidecar ready for ${TRIPLE} in src-tauri/binaries/`);
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
