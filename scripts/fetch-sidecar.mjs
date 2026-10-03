#!/usr/bin/env node
/**
 * Copies ffmpeg / ffprobe into `src-tauri/binaries/` using the Tauri sidecar
 * naming convention (`<name>-<target-triple>`).
 *
 * Resolution order:
 *   1. node_modules/ffmpeg-static + ffprobe-static  (self-contained builds)
 *   2. system PATH                                  (Homebrew, apt, …)
 *
 * Usage:  node scripts/fetch-sidecar.mjs
 */
import { execFileSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, statSync } from "node:fs";
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

if (!TRIPLE) {
  console.error(`Unsupported platform: ${process.platform} ${process.arch}`);
  process.exit(1);
}

function fromPackage(pkg) {
  try {
    const p = require(pkg);
    if (typeof p === "string" && existsSync(p)) return p;
  } catch {
    /* not installed */
  }
  return null;
}

function fromPath(name) {
  try {
    const out = execFileSync(process.platform === "win32" ? "where" : "which", [name], {
      encoding: "utf8",
    }).trim();
    const first = out.split(/\r?\n/)[0];
    return first && existsSync(first) ? first : null;
  } catch {
    return null;
  }
}

const jobs = [
  { name: "ffmpeg", source: fromPackage("ffmpeg-static") || fromPath("ffmpeg") },
  { name: "ffprobe", source: fromPackage("ffprobe-static")?.path || fromPath("ffprobe") },
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
  console.log(`\nSidecars ready for ${TRIPLE} in src-tauri/binaries/`);
}
