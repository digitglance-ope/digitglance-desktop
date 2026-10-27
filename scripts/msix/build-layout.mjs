// Assemble an MSIX package layout directory from a built Tauri app:
//
//   <out>/
//     AppxManifest.xml
//     DigitGlanceTrade.exe        (the Tauri binary, renamed: no spaces)
//     Assets/StoreLogo.png ...    (generated from assets/<product>-icon-source.png)
//
// `makeappx pack /d <out> /p <file>.msix` then turns it into a package.
// The logos are generated from the same 1024x1024 source `tauri icon` uses for
// src-tauri/icons/<product>, so the Store tiles match the installed app icon.

import { copyFileSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { decodePng, encodePng, fitCentered } from "./png.mjs";
import {
  ASSETS,
  PRODUCTS,
  buildManifest,
  exeNameFor,
  resolveIdentity,
  toMsixVersion,
} from "./manifest.mjs";

export const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");

export function readProductConfig(product, root = REPO_ROOT) {
  if (!PRODUCTS.includes(product)) throw new Error(`unknown product "${product}" (trade|books|school)`);
  const base = JSON.parse(readFileSync(join(root, "src-tauri", "tauri.conf.json"), "utf8"));
  const over = JSON.parse(readFileSync(join(root, "src-tauri", `tauri.${product}.conf.json`), "utf8"));
  return {
    productName: over.productName ?? base.productName,
    version: over.version ?? base.version,
    shortDescription: over.bundle?.shortDescription ?? base.bundle?.shortDescription,
  };
}

export function readIdentityFile(path) {
  return existsSync(path) ? JSON.parse(readFileSync(path, "utf8")) : {};
}

export function iconSourceFor(product, root = REPO_ROOT) {
  const own = join(root, "assets", `${product}-icon-source.png`);
  return existsSync(own) ? own : join(root, "assets", "icon-source.png");
}

/** Write the logo set into <assetsDir>; returns the list of files written. */
export function generateAssets(iconPath, assetsDir) {
  const src = decodePng(readFileSync(iconPath));
  mkdirSync(assetsDir, { recursive: true });
  return ASSETS.map((a) => {
    const out = join(assetsDir, a.file);
    writeFileSync(out, encodePng(fitCentered(src, a.width, a.height, a.fill)));
    return out;
  });
}

/**
 * Build the layout. `exe` may be omitted for dry runs/tests, in which case a
 * placeholder file is written so the layout is structurally complete.
 */
export function buildLayout({
  product,
  flavor,
  arch = "x64",
  version,
  exe,
  out,
  env = process.env,
  identityFile = join(REPO_ROOT, "msix", "store-identity.json"),
  root = REPO_ROOT,
}) {
  const cfg = readProductConfig(product, root);
  const appVersion = version || cfg.version;
  const msixVersion = toMsixVersion(appVersion);
  const identity = resolveIdentity({
    product,
    flavor,
    file: readIdentityFile(identityFile),
    env,
    productName: cfg.productName,
  });
  const executable = exeNameFor(cfg.productName);

  rmSync(out, { recursive: true, force: true });
  mkdirSync(out, { recursive: true });
  if (exe) {
    if (!existsSync(exe)) throw new Error(`built app not found: ${exe}`);
    copyFileSync(exe, join(out, executable));
  } else {
    writeFileSync(join(out, executable), "placeholder: no exe supplied (dry run)\n");
  }
  generateAssets(iconSourceFor(product, root), join(out, "Assets"));
  const manifest = buildManifest({
    identity,
    version: msixVersion,
    arch,
    executable,
    description: cfg.shortDescription,
    shortName: cfg.productName,
  });
  writeFileSync(join(out, "AppxManifest.xml"), manifest);
  return { out, appVersion, msixVersion, identity, executable, arch, flavor, product };
}
