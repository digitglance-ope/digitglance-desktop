// MSIX package identity, version mapping and AppxManifest.xml generation.
//
// Tauri's bundler has no MSIX target, so the Store package is assembled from
// the built app (see build-layout.mjs and .github/workflows/store-msix.yml).
// Everything here is pure (no I/O) so it can be unit-tested with node:test.

export const PRODUCTS = ["trade", "books", "school"];
export const ARCHES = ["x64", "arm64"];

// Desktop Bridge minimum: Windows 10 1809. MaxVersionTested = Windows 11 24H2.
export const MIN_VERSION = "10.0.17763.0";
export const MAX_VERSION_TESTED = "10.0.26100.0";

// Logo slots referenced from the manifest, with the exact pixel size the slot
// expects at scale-100 and how much of the canvas the icon mark fills. No
// resources.pri is generated, so these unqualified files are what Windows uses.
export const ASSETS = [
  { file: "StoreLogo.png", width: 50, height: 50, fill: 1.0 },
  { file: "Square44x44Logo.png", width: 44, height: 44, fill: 1.0 },
  { file: "Square71x71Logo.png", width: 71, height: 71, fill: 0.8 },
  { file: "Square150x150Logo.png", width: 150, height: 150, fill: 0.66 },
  { file: "Square310x310Logo.png", width: 310, height: 310, fill: 0.66 },
  { file: "Wide310x150Logo.png", width: 310, height: 150, fill: 0.66 },
  { file: "SplashScreen.png", width: 620, height: 300, fill: 0.66 },
];

// Identity used for the sideload TEST package. Deliberately different from the
// Store identity so a test install never collides with (or is mistaken for)
// the Store app, and so it can be built before Partner Center exists.
export const TEST_PUBLISHER = "CN=DigitGlance-Test-Only";
export const TEST_PUBLISHER_DISPLAY_NAME = "DigitGlance TEST ONLY";

const PLACEHOLDER = /^\s*$|^REPLACE_ME/;

export function isPlaceholder(v) {
  return v === undefined || v === null || PLACEHOLDER.test(String(v));
}

/**
 * App version (semver X.Y.Z) -> MSIX version X.Y.Z.0.
 * The Store reserves the fourth part (must be 0); every part is 0..65535 and
 * the first cannot be 0. Pre-release/build suffixes are not representable.
 */
export function toMsixVersion(appVersion) {
  const m = /^v?(\d+)\.(\d+)\.(\d+)$/.exec(String(appVersion ?? "").trim());
  if (!m) {
    throw new Error(
      `version "${appVersion}" must be plain X.Y.Z (no pre-release suffix) to map to an MSIX version`,
    );
  }
  const parts = m.slice(1, 4).map(Number);
  if (parts.some((p) => p > 65535)) throw new Error(`version "${appVersion}": each part must be <= 65535`);
  if (parts[0] === 0) throw new Error(`version "${appVersion}": the major part cannot be 0 for the Store`);
  return `${parts.join(".")}.0`;
}

/** "DigitGlance Trade" -> "DigitGlanceTrade.exe" (no spaces inside the package). */
export function exeNameFor(productName) {
  return `${String(productName).replace(/[^A-Za-z0-9]/g, "")}.exe`;
}

/**
 * Resolve package identity for one product.
 *  - flavor "store": values from env (repo variables) or msix/store-identity.json;
 *    placeholders are an error, because Partner Center rejects a package whose
 *    identity does not match the reserved app exactly.
 *  - flavor "test": fixed throwaway identity, signed in CI with a self-signed cert.
 */
export function resolveIdentity({ product, flavor, file = {}, env = {}, productName }) {
  if (!PRODUCTS.includes(product)) throw new Error(`unknown product "${product}"`);
  const P = product.toUpperCase();
  const fromFile = (file.products && file.products[product]) || {};
  const pick = (envKey, fileVal) => (!isPlaceholder(env[envKey]) ? env[envKey] : fileVal);

  const storeDisplayName = pick(`MSIX_${P}_DISPLAY_NAME`, fromFile.displayName);
  const displayName = isPlaceholder(storeDisplayName) ? productName : storeDisplayName;

  if (flavor === "test") {
    const cap = product[0].toUpperCase() + product.slice(1);
    return {
      name: `DigitGlance.${cap}.TestOnly`,
      publisher: TEST_PUBLISHER,
      publisherDisplayName: TEST_PUBLISHER_DISPLAY_NAME,
      displayName: `${displayName} (TEST ONLY)`,
    };
  }
  if (flavor !== "store") throw new Error(`unknown flavor "${flavor}" (store|test)`);

  const id = {
    name: pick(`MSIX_${P}_IDENTITY_NAME`, fromFile.identityName),
    publisher: pick("MSIX_PUBLISHER", file.publisher),
    publisherDisplayName: pick("MSIX_PUBLISHER_DISPLAY_NAME", file.publisherDisplayName),
    displayName,
  };
  const missing = [];
  if (isPlaceholder(id.name)) missing.push(`MSIX_${P}_IDENTITY_NAME (Package/Identity/Name)`);
  if (isPlaceholder(id.publisher)) missing.push("MSIX_PUBLISHER (Package/Identity/Publisher)");
  if (isPlaceholder(id.publisherDisplayName)) missing.push("MSIX_PUBLISHER_DISPLAY_NAME (Package/Properties/PublisherDisplayName)");
  if (missing.length) {
    throw new Error(
      `Store identity for "${product}" is not configured. Copy these from Partner Center ` +
        `(App > Product management > Product identity) into repo variables or msix/store-identity.json: ` +
        missing.join(", "),
    );
  }
  validateIdentity(id);
  return id;
}

export function validateIdentity(id) {
  if (!/^[A-Za-z0-9.-]{3,50}$/.test(id.name)) {
    throw new Error(`Identity Name "${id.name}" must be 3-50 chars of A-Z a-z 0-9 . -`);
  }
  if (!/^CN=.+/.test(id.publisher) || id.publisher.length > 8192) {
    throw new Error(`Publisher "${id.publisher}" must be a distinguished name starting with "CN="`);
  }
  for (const [k, v] of [["PublisherDisplayName", id.publisherDisplayName], ["DisplayName", id.displayName]]) {
    if (!v || v.length > 256) throw new Error(`${k} must be 1-256 characters`);
  }
}

export function xmlEscape(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&apos;");
}

/** Build AppxManifest.xml text for a Desktop Bridge (full-trust Win32) app. */
export function buildManifest({ identity, version, arch, executable, description, shortName }) {
  if (!ARCHES.includes(arch)) throw new Error(`unknown arch "${arch}"`);
  if (!/^\d+\.\d+\.\d+\.0$/.test(version)) throw new Error(`MSIX version "${version}" must be X.Y.Z.0`);
  validateIdentity(identity);
  const e = xmlEscape;
  const desc = String(description || identity.displayName).slice(0, 2048);
  const tileName = String(shortName || identity.displayName).slice(0, 40);
  return `<?xml version="1.0" encoding="utf-8"?>
<!-- Generated by scripts/msix/build-layout.mjs. Do not edit by hand. -->
<Package
  xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
  xmlns:uap="http://schemas.microsoft.com/appx/manifest/uap/windows10"
  xmlns:rescap="http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities"
  IgnorableNamespaces="uap rescap">
  <Identity Name="${e(identity.name)}" Publisher="${e(identity.publisher)}" Version="${e(version)}" ProcessorArchitecture="${e(arch)}" />
  <Properties>
    <DisplayName>${e(identity.displayName)}</DisplayName>
    <PublisherDisplayName>${e(identity.publisherDisplayName)}</PublisherDisplayName>
    <Logo>Assets\\StoreLogo.png</Logo>
    <Description>${e(desc)}</Description>
  </Properties>
  <Dependencies>
    <TargetDeviceFamily Name="Windows.Desktop" MinVersion="${MIN_VERSION}" MaxVersionTested="${MAX_VERSION_TESTED}" />
  </Dependencies>
  <Resources>
    <Resource Language="en-us" />
  </Resources>
  <Applications>
    <Application Id="App" Executable="${e(executable)}" EntryPoint="Windows.FullTrustApplication">
      <uap:VisualElements
        DisplayName="${e(identity.displayName)}"
        Description="${e(desc)}"
        BackgroundColor="transparent"
        Square150x150Logo="Assets\\Square150x150Logo.png"
        Square44x44Logo="Assets\\Square44x44Logo.png">
        <uap:DefaultTile
          Wide310x150Logo="Assets\\Wide310x150Logo.png"
          Square71x71Logo="Assets\\Square71x71Logo.png"
          Square310x310Logo="Assets\\Square310x310Logo.png"
          ShortName="${e(tileName)}">
          <uap:ShowNameOnTiles>
            <uap:ShowOn Tile="square150x150Logo" />
            <uap:ShowOn Tile="wide310x150Logo" />
            <uap:ShowOn Tile="square310x310Logo" />
          </uap:ShowNameOnTiles>
        </uap:DefaultTile>
        <uap:SplashScreen Image="Assets\\SplashScreen.png" />
      </uap:VisualElements>
    </Application>
  </Applications>
  <Capabilities>
    <Capability Name="internetClient" />
    <rescap:Capability Name="runFullTrust" />
  </Capabilities>
</Package>
`;
}
