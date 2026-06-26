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

Installers are built in CI by [`.github/workflows/release.yml`](.github/workflows/release.yml).
Push a tag `v<semver>` (e.g. `v1.0.0`) to build Windows + macOS installers and a
signed update feed (`latest.json`) into a **draft** GitHub Release for review.

Local build:

```bash
npm install
npm run icons     # generate the icon set from assets/icon-source.png
npm run build     # or: npm run dev
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
- **OS code-signing** (follow-up): installers are currently unsigned, so first
  run shows a SmartScreen (Windows) / Gatekeeper (macOS) prompt. Add an Apple
  Developer ID + Windows OV/EV certificate to remove it.
