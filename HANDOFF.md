# Per-product desktop apps — handoff

This repo now builds **three** DigitGlance desktop apps from one shared Tauri
shell, one per product. Each app is a thin, hardened native window onto the live
web product — no business logic, no privileged IPC to the remote origin — exactly
like the old unified app, but branded and locked to a single product.

| App | Identifier (permanent) | Lands on | Release tag | Updater endpoint tag |
|-----|------------------------|----------|-------------|----------------------|
| DigitGlance Trade | `com.digitglance.trade` | `https://digitglance.com/app` | `trade-v*` | `trade-latest` |
| DigitGlance Books | `com.digitglance.books` | `https://digitglance.com/app/accounting` | `books-v*` | `books-latest` |
| DigitGlance School | `com.digitglance.school` | `https://school.digitglance.com/app` | `school-v*` | `school-latest` |

## What changed

- **`src-tauri/tauri.<product>.conf.json`** — one small override file per product
  (product name, identifier, version, icons, landing gate, updater endpoint). Each
  is deep-merged over the shared base `tauri.conf.json` at build time, the same
  pattern the Android config already uses.
- **`dist/<product>/index.html`** — the connectivity gate per product, carrying
  that product's title and landing URL. Offline behaviour is unchanged.
- **`.github/workflows/release-products.yml`** — builds one product (derived from
  the pushed tag) for Windows + macOS and drafts a per-product release.
- **`.github/workflows/promote-latest.yml`** — turns on auto-update by copying a
  published release's `latest.json` into the moving `<product>-latest` release.
- **`package.json`** — `dev:<product>`, `build:<product>`, `icons:<product>`.

The old `release.yml` (`v*` tags, unified app) is superseded and can be removed
once the three streams are live.

## Build a product

Local (needs Rust): `npm run build:trade` (or `:books` / `:school`).
CI: push a tag, e.g. `git tag trade-v1.0.0 && git push origin trade-v1.0.0`, or
run **Release (per product)** from the Actions tab and pick the product.

Windows is the shippable target. macOS builds but is unsigned and the dmg step is
flaky on headless runners — re-run failed jobs if needed, and keep it out of the
public download page until it is signed + notarised.

## Publish + enable auto-update

1. CI drafts `DigitGlance Trade trade-v1.0.0` with the signed `.exe` + `latest.json`.
2. Review and **publish** the draft. The marketing site's `/download` and the
   Trade product page pick it up automatically (they match the `trade-v` tag).
3. Run **Promote updater manifest** (product = trade, version_tag = trade-v1.0.0)
   so installed apps auto-update from `trade-latest/latest.json`.

## Before the first release — decisions to confirm

- **Identifiers are permanent** once the updater ships. `com.digitglance.trade` /
  `.books` / `.school` are the proposed values.
- **Per-product icons.** Each app now has its own mark from
  `assets/<product>-icon-source.png` (teal receipt for Trade, blue open book for
  Books, green cap for School); the CI icon step falls back to the shared source
  if a product one is missing. Replace those 1024x1024 PNGs to restyle. The
  per-product splash logo in `dist/<product>/logo.png` can also be replaced.
- **Landing URLs** above are sensible defaults; pin Trade to `/app/pos` or
  `/app/invoice` if you want it to skip the hub.
- **Signing.** One updater key signs all three (already configured via the
  `TAURI_SIGNING_PRIVATE_KEY` secrets). OS code-signing (OV/EV cert for Windows,
  Apple Developer for macOS) is still pending and unchanged by this work.

## What could not be validated here

This machine has no Rust toolchain, so no Tauri build was run locally. The config
merge, CI matrix, and updater endpoints are wired to the proven existing setup,
but the **first CI build is the validation gate** — run one product to green
before tagging the others.
