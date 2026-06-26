# Building the DigitGlance Android app (Play Store AAB)

This repo's Tauri shell targets **desktop** (Windows/macOS via `release.yml`) **and
Android**. This guide covers producing a signed **Android App Bundle (`.aab`)** for
the Google Play Store.

> **Why a build machine / CI, not your laptop alone:** an Android release needs the
> Android SDK + NDK + JDK 17 and a signing keystore. The
> [`android.yml`](.github/workflows/android.yml) workflow sets all of that up and
> produces a **signed `.aab`** as a downloadable artifact. You can also build
> locally (see below) if your machine has the toolchain.

---

## TL;DR (recommended path)

1. **Once:** create an upload keystore (below) and add the four signing secrets.
2. Run the **Android (Play Store AAB)** workflow (Actions tab → Run workflow), or
   push a tag `android-vX.Y.Z`.
3. Download the `digitglance-android-aab` artifact → upload it in the Play Console.

---

## 1. One-time: create an upload keystore

Google Play uses **Play App Signing** — you sign each upload with an *upload key*,
Google re-signs with the *app signing key*. Generate the upload key once and keep it
safe (losing it means resetting the upload key with Google support):

```bash
keytool -genkeypair -v \
  -keystore digitglance-upload.jks \
  -alias digitglance \
  -keyalg RSA -keysize 2048 -validity 9125 \
  -storepass "<STORE_PASSWORD>" -keypass "<KEY_PASSWORD>" \
  -dname "CN=Digitglance Reliance, O=Digitglance Reliance, L=Lagos, C=NG"
```

Back this `.jks` up somewhere secure (password manager / offline). **Do not commit it.**

## 2. One-time: add GitHub repo secrets

Settings → Secrets and variables → Actions → New repository secret:

| Secret | Value |
| --- | --- |
| `ANDROID_KEYSTORE_BASE64` | `base64 -w0 digitglance-upload.jks` (the whole file, base64) |
| `ANDROID_KEYSTORE_PASSWORD` | the `-storepass` you used |
| `ANDROID_KEY_ALIAS` | `digitglance` (the `-alias`) |
| `ANDROID_KEY_PASSWORD` | the `-keypass` you used |

> On macOS use `base64 -i digitglance-upload.jks | tr -d '\n'` to get a single line.

If these are absent the workflow still runs but uploads an **unsigned** `.aab`
(useful to verify the build, **not** uploadable to Play).

## 3. Build the AAB

**Via CI (recommended):** Actions → *Android (Play Store AAB)* → **Run workflow**
(or push a tag `android-vX.Y.Z`). Download the `digitglance-android-aab` artifact.

**Locally (optional, needs the toolchain):**

```bash
# Prereqs: JDK 17, Android SDK + NDK (r26), Rust + Android targets:
rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android
export NDK_HOME="$ANDROID_HOME/ndk/26.1.10909125"

npm install
npm run android:init     # generates src-tauri/gen/android (gitignored)
npm run android:build    # produces the .aab under src-tauri/gen/android/.../bundle/
```

Then sign it (same as CI):

```bash
jarsigner -sigalg SHA256withRSA -digestalg SHA-256 \
  -keystore digitglance-upload.jks -storepass "<STORE_PASSWORD>" -keypass "<KEY_PASSWORD>" \
  path/to/app-universal-release.aab digitglance
```

## 4. Upload to the Play Console

Create the app in the [Play Console](https://play.google.com/console), enable
**Play App Signing**, and upload the `.aab` to an Internal testing track first.

---

## Important notes

- **`applicationId` is permanent.** The Android package is **`com.digitglance.app`**
  (set in [`src-tauri/tauri.android.conf.json`](src-tauri/tauri.android.conf.json) so
  it stays separate from the desktop updater's `com.digitglance.desktop`). It can
  **never change** once published — confirm it before the first upload.
- **Bump the version for every upload.** Play requires a strictly increasing
  `versionCode`; Tauri derives it from `version` in `tauri.conf.json`. Increment
  `version` (e.g. `1.0.0` → `1.0.1`) before each release.
- **Target API level.** The workflow installs `platforms;android-34`; keep this at or
  above Google's current minimum target API for new uploads.
- **Play "minimum functionality" policy.** This app is a native shell around the live
  web app (`digitglance.com/app/dashboard`). Google can reject apps that are *only* a
  thin webview wrapper. The shell adds offline handling and a native window; before
  submitting, make sure the web app is mobile-responsive and consider adding native
  value (push notifications, share targets, etc.) to clearly meet the policy.
- **Icons** are regenerated from `assets/icon-source.png` by `tauri android init`, so
  the Android launcher icon matches the official brand mark automatically.

## Troubleshooting

- *NDK not found* → ensure `NDK_HOME` points at an installed NDK (`$ANDROID_HOME/ndk/<version>`).
- *No `.aab` produced* → re-run `npm run android:init` after changing `tauri.conf.json`;
  `gen/android` is regenerated, not committed.
- *`jarsigner` "no manifest"* → you pointed it at the wrong file; sign the
  `*-release.aab`, not an intermediate.
