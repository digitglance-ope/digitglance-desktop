#!/usr/bin/env node
// MSIX helper CLI used by .github/workflows/store-msix.yml (and runnable locally).
//
//   node scripts/msix/cli.mjs version  --product trade [--version 1.0.3]
//       -> prints app_version / msix_version (and writes them to $GITHUB_OUTPUT)
//   node scripts/msix/cli.mjs identity --product trade
//       -> exit 0 if the Store identity is configured, 2 (with the reason) if not
//   node scripts/msix/cli.mjs layout   --product trade --flavor store|test
//                                      --out build/msix/trade-store [--arch x64]
//                                      [--version 1.0.3] [--exe path/to/app.exe]
//       -> writes AppxManifest.xml + Assets/ + the renamed exe into --out

import { appendFileSync } from "node:fs";
import { join } from "node:path";
import { REPO_ROOT, buildLayout, readIdentityFile, readProductConfig } from "./build-layout.mjs";
import { resolveIdentity, toMsixVersion } from "./manifest.mjs";

function parseArgs(argv) {
  const [cmd, ...rest] = argv;
  const opts = {};
  for (let i = 0; i < rest.length; i++) {
    const k = rest[i];
    if (!k.startsWith("--")) throw new Error(`unexpected argument "${k}"`);
    const v = rest[i + 1];
    if (v === undefined || v.startsWith("--")) throw new Error(`missing value for ${k}`);
    opts[k.slice(2)] = v;
    i++;
  }
  return { cmd, opts };
}

function ghOutput(pairs) {
  for (const [k, v] of Object.entries(pairs)) console.log(`${k}=${v}`);
  if (process.env.GITHUB_OUTPUT) {
    appendFileSync(process.env.GITHUB_OUTPUT, Object.entries(pairs).map(([k, v]) => `${k}=${v}\n`).join(""));
  }
}

function main() {
  const { cmd, opts } = parseArgs(process.argv.slice(2));
  const product = opts.product;
  if (cmd === "version") {
    const cfg = readProductConfig(product);
    const appVersion = (opts.version || "").trim().replace(/^v/, "") || cfg.version;
    ghOutput({ app_version: appVersion, msix_version: toMsixVersion(appVersion), product_name: cfg.productName });
    return 0;
  }
  if (cmd === "identity") {
    const cfg = readProductConfig(product);
    try {
      const id = resolveIdentity({
        product,
        flavor: "store",
        file: readIdentityFile(join(REPO_ROOT, "msix", "store-identity.json")),
        env: process.env,
        productName: cfg.productName,
      });
      console.log(`Store identity OK: ${id.name} / ${id.publisher} / ${id.publisherDisplayName} / "${id.displayName}"`);
      return 0;
    } catch (e) {
      console.error(e.message);
      return 2;
    }
  }
  if (cmd === "layout") {
    if (!opts.out) throw new Error("--out is required");
    const r = buildLayout({
      product,
      flavor: opts.flavor,
      arch: opts.arch || "x64",
      version: opts.version,
      exe: opts.exe,
      out: opts.out,
    });
    console.log(
      `Layout ready: ${r.out} (${r.flavor}, ${r.arch}, ${r.identity.name} ${r.msixVersion}, exe ${r.executable})`,
    );
    return 0;
  }
  console.error("usage: cli.mjs version|identity|layout --product trade|books|school [...]");
  return 1;
}

try {
  process.exitCode = main();
} catch (e) {
  console.error(`error: ${e.message}`);
  process.exitCode = 1;
}
