# DigitGlance Desktop

Public **distribution** repo for the DigitGlance desktop app (Windows + macOS),
plus the **Android** (Play Store) build of the same Tauri shell.

It holds only the **non-sensitive desktop shell** (a [Tauri 2](https://tauri.app)
wrapper around the live web app) and the **published installers + auto-update
feed**. The rest of the product — website, backend, business logic — lives in a
**private** repository and is **not** here.

## Download

Get the latest installer from the [**Releases**](https://github.com/digitglance-ope/digitglance-desktop/releases)
page, or from **https://digitglance.com/download**.

## What this is (and isn't)

- **Is:** a thin native window that loads `https://digitglance.com/app/dashboard`.
  All authentication, data, reports, AI, billing and permissions run server-side
  in the live app — the shell adds no business logic and stores no business data
  locally.
- **Isn't:** the application source. There are no API keys, credentials, or
  backend logic in this repo (that's why it can safely be public).

## Build & release

This one shell builds **three separate single-product apps** — DigitGlance Trade,
DigitGlance Books and DigitGlance School — each with its own window title, bundle
identifier, icons, landing page and update feed, taken from
`src-tauri/tauri.<product>.conf.json` merged over the base `tauri.conf.json`.

Installers are built in CI by
[`.github/workflows/release-products.yml`](.github/workflows/release-products.yml).
Push a product tag `<product>-v<semver>` (e.g. `trade-v1.0.2`) to build Windows +
macOS installers and a signed update feed (`latest.json`) into a **draft** GitHub
Release for review.

Shipping takes three steps, not one — a tag alone reaches nobody:

1. **Tag** `<product>-v<semver>` → CI drafts the release.
2. **Publish** the draft (`gh release edit <tag> --draft=false`) so the assets are
   downloadable and the marketing site's `/download` can see them.
3. **Promote** the manifest — run **Promote updater manifest** with that tag. This
   is the step that updates already-installed apps, because each app polls the
   stable `<product>-latest/latest.json` endpoint, not the versioned one.

Products stay isolated: each app is sold on its own and announces which one it is
with `?client=<product>-desktop` on its landing URL, so the web app hides
cross-product switching and refuses to render the others. See `src-tauri/src/lib.rs`.

Local build:

```bash
npm install
npm run icons:trade    # generate that product's icon set
npm run build:trade    # or :books / :school — or npm run dev:trade
```

### Microsoft Store (MSIX)

Windows customers get each app from the **Microsoft Store as an MSIX package**.
Microsoft signs the package when it publishes it, so no code-signing
certificate is needed for that route. Run **Microsoft Store (MSIX)**
([`.github/workflows/store-msix.yml`](.github/workflows/store-msix.yml)) from
the Actions tab and pick the product. It produces an unsigned `.msix` for
Partner Center and a self-signed **TEST ONLY** `.msix` + `.cer` for trying it
on a real machine first. Store builds use `--features store` +
`src-tauri/tauri.store.conf.json`, which leave out the Tauri self-updater
because the Store delivers updates. The NSIS download and its GitHub updater
are unchanged. Owner steps, the Partner Center identity variables, the WebView2
trade-off and the "uninstall deletes local data" rule for the offline POS are
in [**docs/STORE.md**](docs/STORE.md).

```bash
npm run test:msix          # manifest/asset generator tests (no Rust needed)
npm run build:store:trade  # local Store-flavour binary (needs Rust)
```

### Android (Play Store)

The same shell also builds an Android **App Bundle (`.aab`)** for the Play Store
via [`.github/workflows/android.yml`](.github/workflows/android.yml). See
[**ANDROID_BUILD.md**](ANDROID_BUILD.md) for the keystore setup, required secrets,
and submission notes. The mobile build uses its own `applicationId`
(`com.digitglance.app`) so it stays independent of the desktop updater identifier.

### Signing

- **Auto-update signing** (required, configured): releases are signed with the
  Tauri updater key via the `TAURI_SIGNING_PRIVATE_KEY` /
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` repo secrets; the public key is in
  `src-tauri/tauri.conf.json`.
- **OS code-signing** (follow-up): the direct-download installers are
  currently unsigned, so first run shows a SmartScreen (Windows) / Gatekeeper
  (macOS) prompt. On Windows the answer is the Microsoft Store: the Store signs
  the MSIX (see docs/STORE.md), so no OV/EV certificate is planned. macOS still
  needs an Apple Developer ID.
