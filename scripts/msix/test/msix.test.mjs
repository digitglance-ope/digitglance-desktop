// node --test scripts/msix/test
//
// Validates the MSIX pieces that can be checked without a Rust toolchain:
// version mapping, identity resolution, the generated AppxManifest.xml
// (well-formed + every element/attribute Partner Center and Desktop Bridge
// need) and the generated logo assets (exact pixel sizes). If the Windows SDK's
// makeappx.exe is present (any Windows dev box or the windows-latest runner),
// it also packs each layout, which schema-validates the manifest for real.

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { buildLayout, readProductConfig } from "../build-layout.mjs";
import {
  ASSETS,
  MIN_VERSION,
  PRODUCTS,
  buildManifest,
  exeNameFor,
  isPlaceholder,
  resolveIdentity,
  toMsixVersion,
  xmlEscape,
} from "../manifest.mjs";
import { decodePng, encodePng, fitCentered, readPngSize, resize } from "../png.mjs";
import { find, parseXml, textOf } from "./xml.mjs";

const FAKE_STORE_ENV = {
  MSIX_PUBLISHER: "CN=11111111-2222-3333-4444-555555555555",
  MSIX_PUBLISHER_DISPLAY_NAME: "Digitglance Reliance",
  MSIX_TRADE_IDENTITY_NAME: "12345DigitglanceReliance.DigitGlanceTrade",
  MSIX_BOOKS_IDENTITY_NAME: "12345DigitglanceReliance.DigitGlanceBooks",
  MSIX_SCHOOL_IDENTITY_NAME: "12345DigitglanceReliance.DigitGlanceSchool",
};

function tmp() {
  return mkdtempSync(join(tmpdir(), "dg-msix-"));
}

// ---------------------------------------------------------------- versions

test("app version maps to a Store-legal four-part version ending in .0", () => {
  assert.equal(toMsixVersion("1.0.2"), "1.0.2.0");
  assert.equal(toMsixVersion("v2.13.65535"), "2.13.65535.0");
  for (const bad of ["1.0", "1.0.0.1", "1.0.0-beta.1", "0.9.0", "1.70000.0", "", undefined]) {
    assert.throws(() => toMsixVersion(bad), `${bad} should be rejected`);
  }
});

test("every product config version is mappable", () => {
  for (const p of PRODUCTS) assert.match(toMsixVersion(readProductConfig(p).version), /^\d+\.\d+\.\d+\.0$/);
});

// ---------------------------------------------------------------- identity

test("store identity refuses placeholders and names what is missing", () => {
  const file = JSON.parse(readFileSync(new URL("../../../msix/store-identity.json", import.meta.url), "utf8"));
  assert.ok(isPlaceholder(file.publisher), "committed file must keep placeholders until the owner fills it");
  assert.throws(
    () => resolveIdentity({ product: "trade", flavor: "store", file, env: {}, productName: "DigitGlance Trade" }),
    /MSIX_TRADE_IDENTITY_NAME.*MSIX_PUBLISHER.*MSIX_PUBLISHER_DISPLAY_NAME/s,
  );
});

test("store identity comes from env (repo variables) over the JSON file", () => {
  const id = resolveIdentity({
    product: "books",
    flavor: "store",
    file: { publisher: "CN=from-file", publisherDisplayName: "File Co", products: { books: { identityName: "File.Books" } } },
    env: FAKE_STORE_ENV,
    productName: "DigitGlance Books",
  });
  assert.equal(id.name, FAKE_STORE_ENV.MSIX_BOOKS_IDENTITY_NAME);
  assert.equal(id.publisher, FAKE_STORE_ENV.MSIX_PUBLISHER);
  assert.equal(id.displayName, "DigitGlance Books");
});

test("store identity rejects malformed values", () => {
  const env = { ...FAKE_STORE_ENV, MSIX_PUBLISHER: "Digitglance Reliance" };
  assert.throws(() => resolveIdentity({ product: "trade", flavor: "store", env, productName: "X" }), /CN=/);
  const env2 = { ...FAKE_STORE_ENV, MSIX_TRADE_IDENTITY_NAME: "has spaces in it" };
  assert.throws(() => resolveIdentity({ product: "trade", flavor: "store", env: env2, productName: "X" }), /3-50/);
});

test("test identity is distinct from the Store one and clearly marked", () => {
  const id = resolveIdentity({ product: "school", flavor: "test", env: FAKE_STORE_ENV, productName: "DigitGlance School" });
  assert.equal(id.name, "DigitGlance.School.TestOnly");
  assert.match(id.publisher, /^CN=DigitGlance-Test-Only$/);
  assert.match(id.displayName, /\(TEST ONLY\)$/);
});

// ---------------------------------------------------------------- png

test("png encode/decode round-trips and resize keeps flat colour", () => {
  const w = 64, h = 32;
  const data = new Uint8Array(w * h * 4);
  for (let i = 0; i < w * h; i++) data.set([10, 200, 90, 255], i * 4);
  const back = decodePng(encodePng({ width: w, height: h, data }));
  assert.equal(back.width, w);
  assert.deepEqual(Array.from(back.data.subarray(0, 4)), [10, 200, 90, 255]);
  const small = resize(back, 7, 5);
  assert.deepEqual(Array.from(small.data.subarray(16, 20)), [10, 200, 90, 255]);
  const wide = fitCentered(back, 310, 150, 0.66);
  assert.equal(wide.data[3], 0, "corner of the wide canvas stays transparent");
});

test("icon sources decode (8-bit RGBA, 1024x1024)", () => {
  for (const p of PRODUCTS) {
    const img = decodePng(readFileSync(new URL(`../../../assets/${p}-icon-source.png`, import.meta.url)));
    assert.equal(img.width, 1024);
    assert.equal(img.height, 1024);
  }
});

// ---------------------------------------------------------------- manifest

test("xmlEscape covers the five XML specials", () => {
  assert.equal(xmlEscape(`A&B <"x'>`), "A&amp;B &lt;&quot;x&apos;&gt;");
});

test("manifest escapes hostile display text and stays well-formed", () => {
  const xml = buildManifest({
    identity: { name: "A.B", publisher: 'CN=Q&A, O="X"', publisherDisplayName: "Q & A <Ltd>", displayName: "Tom & Jerry's" },
    version: "1.2.3.0",
    arch: "x64",
    executable: "App.exe",
    description: "Sales & stock <fast>",
  });
  const doc = parseXml(xml);
  assert.equal(textOf(find(doc, "Properties/PublisherDisplayName")), "Q & A <Ltd>");
  assert.equal(find(doc, "Identity").attrs.Publisher, 'CN=Q&A, O="X"');
});

for (const product of PRODUCTS) {
  for (const flavor of ["store", "test"]) {
    test(`${product}/${flavor}: layout has a complete Desktop Bridge manifest and exact-size assets`, () => {
      const out = join(tmp(), "layout");
      try {
        const r = buildLayout({ product, flavor, out, env: FAKE_STORE_ENV, version: "1.4.7" });
        const cfg = readProductConfig(product);
        const doc = parseXml(readFileSync(join(out, "AppxManifest.xml"), "utf8"));

        assert.equal(doc.name, "Package");
        assert.equal(doc.attrs.xmlns, "http://schemas.microsoft.com/appx/manifest/foundation/windows10");
        assert.equal(doc.attrs["xmlns:uap"], "http://schemas.microsoft.com/appx/manifest/uap/windows10");
        assert.equal(
          doc.attrs["xmlns:rescap"],
          "http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities",
        );

        const identity = find(doc, "Identity");
        assert.equal(identity.attrs.Version, "1.4.7.0");
        assert.equal(identity.attrs.ProcessorArchitecture, "x64");
        if (flavor === "store") {
          assert.equal(identity.attrs.Name, FAKE_STORE_ENV[`MSIX_${product.toUpperCase()}_IDENTITY_NAME`]);
          assert.equal(identity.attrs.Publisher, FAKE_STORE_ENV.MSIX_PUBLISHER);
          assert.equal(textOf(find(doc, "Properties/DisplayName")), cfg.productName);
          assert.equal(textOf(find(doc, "Properties/PublisherDisplayName")), "Digitglance Reliance");
        } else {
          assert.match(identity.attrs.Name, /TestOnly$/);
          assert.match(textOf(find(doc, "Properties/DisplayName")), /TEST ONLY/);
        }
        assert.equal(textOf(find(doc, "Properties/Logo")), "Assets\\StoreLogo.png");

        const tdf = find(doc, "Dependencies/TargetDeviceFamily");
        assert.equal(tdf.attrs.Name, "Windows.Desktop");
        assert.equal(tdf.attrs.MinVersion, MIN_VERSION);
        assert.equal(MIN_VERSION, "10.0.17763.0");
        assert.ok(tdf.attrs.MaxVersionTested);
        assert.ok(find(doc, "Resources/Resource").attrs.Language);

        const app = find(doc, "Applications/Application");
        assert.equal(app.attrs.Executable, exeNameFor(cfg.productName));
        assert.equal(app.attrs.EntryPoint, "Windows.FullTrustApplication");
        assert.ok(existsSync(join(out, app.attrs.Executable)), "executable is in the layout");
        assert.doesNotMatch(app.attrs.Executable, /\s/);

        const ve = find(app, "uap:VisualElements");
        assert.ok(ve.attrs.DisplayName && ve.attrs.Description && ve.attrs.BackgroundColor);
        const tile = find(ve, "uap:DefaultTile");
        const splash = find(ve, "uap:SplashScreen");
        assert.ok(tile.attrs.ShortName.length <= 40);
        const refs = {
          "StoreLogo.png": textOf(find(doc, "Properties/Logo")),
          "Square44x44Logo.png": ve.attrs.Square44x44Logo,
          "Square150x150Logo.png": ve.attrs.Square150x150Logo,
          "Wide310x150Logo.png": tile.attrs.Wide310x150Logo,
          "Square71x71Logo.png": tile.attrs.Square71x71Logo,
          "Square310x310Logo.png": tile.attrs.Square310x310Logo,
          "SplashScreen.png": splash.attrs.Image,
        };
        for (const a of ASSETS) {
          assert.equal(refs[a.file], `Assets\\${a.file}`, `${a.file} referenced from the manifest`);
          const size = readPngSize(readFileSync(join(out, "Assets", a.file)));
          assert.deepEqual(size, { width: a.width, height: a.height }, `${a.file} size`);
        }
        assert.deepEqual(readdirSync(join(out, "Assets")).sort(), ASSETS.map((a) => a.file).sort());

        const caps = find(doc, "Capabilities").children.filter((c) => c.name);
        assert.ok(caps.some((c) => c.name === "Capability" && c.attrs.Name === "internetClient"));
        assert.ok(caps.some((c) => c.name === "rescap:Capability" && c.attrs.Name === "runFullTrust"));
        assert.equal(r.msixVersion, "1.4.7.0");
      } finally {
        rmSync(join(out, ".."), { recursive: true, force: true });
      }
    });
  }
}

test("store layout without configured identity fails with guidance", () => {
  const out = join(tmp(), "layout");
  try {
    assert.throws(
      () => buildLayout({ product: "trade", flavor: "store", out, env: {} }),
      /Partner Center/,
    );
  } finally {
    rmSync(join(out, ".."), { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------- makeappx (optional)

function findMakeAppx() {
  if (process.platform !== "win32") return null;
  const root = "C:\\Program Files (x86)\\Windows Kits\\10\\bin";
  if (!existsSync(root)) return null;
  const versions = readdirSync(root).filter((d) => /^10\./.test(d)).sort((a, b) =>
    a.localeCompare(b, undefined, { numeric: true }),
  );
  for (const v of versions.reverse()) {
    const p = join(root, v, "x64", "makeappx.exe");
    if (existsSync(p)) return p;
  }
  return null;
}

const makeappx = findMakeAppx();
test("makeappx accepts the generated layout (schema validation)", { skip: !makeappx && "makeappx.exe not found" }, () => {
  const dir = tmp();
  try {
    for (const flavor of ["store", "test"]) {
      const out = join(dir, flavor);
      buildLayout({ product: "trade", flavor, out, env: FAKE_STORE_ENV, version: "1.0.2" });
      const pkg = join(dir, `trade-${flavor}.msix`);
      execFileSync(makeappx, ["pack", "/o", "/h", "SHA256", "/d", out, "/p", pkg], { stdio: "pipe" });
      assert.ok(existsSync(pkg));
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
