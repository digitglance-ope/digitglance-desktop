# Per-product desktop apps — handoff

This repo now builds **three** DigitGlance desktop apps from one shared Tauri
shell, one per product. Each app is a thin, hardened native window onto the live
web product — no business logic, no privileged IPC to the remote origin — exactly
like the old unified app, but branded and locked to a single product.

| App | Identifier (permanent) | Lands on | Release tag | Updater endpoint tag |
|-----|------------------------|----------|-------------|----------------------|
| DigitGlance Trade | `com.digitglance.trade` | `https://digitglance.com/app` | `trade-v*` | `trade-latest` |
| DigitGlance Books | `com.digitglance.books` | `https://digitglance.com/app/accounting` | `books-v*` | `books-latest` |
| DigitGlance School | `com.digitglance.school` | `https://school.digitglance.com/app/login?client=school-desktop` | `school-v*` | `school-latest` |

## What changed

- **`src-tauri/tauri.<product>.conf.json`** — one small override file per product
  (product name, identifier, version, icons, landing gate, updater endpoint). Each
  is deep-merged over the shared base `tauri.conf.json` at build time, the same
  pattern the Android config already uses.
- **`dist/<product>/index.html`** — the connectivity gate per product, carrying
  that product's title and landing URL. Offline behaviour is unchanged.
  School lands on the **sign-in form**, never sign-up, and carries
  `?client=school-desktop`: DigitGlance School is licensed to a school as an
  institution and sold only on the web, so the web app reads that signal to hide
  account creation and every billing surface in the shell. Do not drop the param,
  and do not point School at a marketing or sign-up page.
- **`.github/workflows/release-products.yml`** — builds one product (derived from
  the pushed tag) for Windows + macOS and drafts a per-product release.
- **`.github/workflows/promote-latest.yml`** — turns on auto-update by copying a
  published release's `latest.json` into the moving `<product>-latest` release.
- **`package.json`** — `dev:<product>`, `build:<product>`, `icons:<product>`.

The old `release.yml` (`v*` tags, unified app) has been REMOVED, now that all
three streams are live. It built a single all-products app that opened on the
web hub, which is exactly the cross-product leak the per-product split exists to
prevent — so leaving it dispatchable meant one stray `v*` tag could ship an app
that bypassed the isolation. Nothing was ever published from it (no `v*` tag or
release existed), so no such app is installed anywhere. Recover it from git
history if the unified app is ever wanted back.

## Build a product

Local (needs Rust): `npm run build:trade` (or `:books` / `:school`).
CI: push a tag, e.g. `git tag trade-v1.0.0 && git push origin trade-v1.0.0`, or
run **Release (per product)** from the Actions tab and pick the product.

Windows is the shippable target. macOS builds but is unsigned and the dmg step is
flaky on headless runners — re-run failed jobs if needed, and keep it out of the
public download page until it is signed + notarised.

## Microsoft Store (MSIX)

Windows distribution goes through the Microsoft Store as MSIX (owner decision:
the Store signs the package, so no code-signing certificate is bought).
`.github/workflows/store-msix.yml` (manual: product, version, optional arm64)
builds the binary with `--features store` + `src-tauri/tauri.store.conf.json`
(no Tauri updater), writes `AppxManifest.xml` + logo assets with
`scripts/msix/`, and runs `makeappx pack`. Artifacts:
`<product>-<version>-x64-store.msix` (unsigned, for Partner Center; built once
the `MSIX_*` repo variables hold the Partner Center identity) and
`<product>-<version>-x64-test.msix` + `.cer` (TEST ONLY, self-signed in CI).
Full owner checklist, WebView2 choice, and the rule that **uninstalling an MSIX
deletes its local data** (offline POS: never uninstall with pending sales) are
in [docs/STORE.md](docs/STORE.md). The NSIS path below is unchanged.

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
  `TAURI_SIGNING_PRIVATE_KEY` secrets). Windows: Store MSIX is signed by
  Microsoft (docs/STORE.md); the direct NSIS download stays unsigned. macOS:
  Apple Developer ID still pending.
- **Store identity.** Reserve the three names in Partner Center and copy the
  Package Identity values into the `MSIX_*` repo variables (docs/STORE.md).

## What could not be validated here

This machine has no Rust toolchain, so no Tauri build was run locally. The config
merge, CI matrix, and updater endpoints are wired to the proven existing setup,
but the **first CI build is the validation gate** — run one product to green
before tagging the others.

## Offline till local store (Trade)

`src-tauri/src/store` is the native local store for the offline POS till:
SQLCipher-encrypted SQLite (key in the Windows Credential Manager, one file per
business and device), the upload queue, retention cleanup, the storage monitor
and offline PIN checks, exposed as Tauri commands. The contract, security model,
durability rules, test list and timings are in `docs/LOCAL-STORE-API.md`.

- The commands are declared in `src-tauri/build.rs` and granted only by
  `capabilities/local-store.json` (main window, local pages, no remote origin).
  Adding a command means adding it in four places; a test checks they agree.
- Builds now compile OpenSSL from source for SQLCipher, which needs a native Perl
  (Strawberry on Windows; the release workflow points `OPENSSL_SRC_PERL` at it).
- `.github/workflows/local-store-tests.yml` runs the store tests and clippy on
  Windows for every PR touching `src-tauri/`.
- Only DigitGlance Trade (and the base dev config) enable the store; Books and
  School reject the commands.
