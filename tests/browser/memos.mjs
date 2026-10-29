import assert from "node:assert/strict";
import { readFile, mkdir } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const root = new URL("../../", import.meta.url);
const assets = {};
for (const [url, path, contentType] of [["/", "web/index.html", "text/html"], ["/app.js", "web/app.js", "text/javascript"], ["/style.css", "web/style.css", "text/css"]]) {
  assets[url] = { body: await readFile(new URL(path, root), "utf8"), contentType };
}
const browser = await chromium.launch({ headless: true, ...(process.env.MDDIFF_BROWSER_CHANNEL ? { channel: process.env.MDDIFF_BROWSER_CHANNEL } : {}) });
let phase = "setup";
try {
  const page = await browser.newPage({ viewport: { width: 1200, height: 850 } });
  let authenticated = true;
  let csrf = "synthetic-session-a";
  let enabled = true;
  let result = "success";
  let releaseRead;
  let enteredRead;
  let readBarrier = null;
  const markdown = "\ufeff# Nota sintética\r\n\r\nContenido con acentos: áéíóú 📝\r\nSin salto final";
  const candidate = { label: "Propuesta actual.md", markdown: "# Propuesta\r\nSe conserva." };
  const work = { revision: 1, base: { label: "Original.md", markdown: "# Original" }, candidate, base_source: null };
  const reads = [];
  const writes = [];
  const destinations = [
    { id: "notes", provider: "memos", label: "Notas de prueba", url: "https://notes.example", connected: true, revision: 1 },
    { id: "other", provider: "memos", label: "Notas sin conectar", url: "https://other.example", connected: false, revision: 0 },
    { id: "issues", provider: "jira", label: "Jira de prueba", url: "https://issues.example", connected: true, revision: 1 },
  ];
  const block = (document) => document && ({ source: document.markdown, html: "<p>Contenido sintético</p>", start_line: 1, end_line: 3 });
  const snapshot = () => ({
    revision: work.revision, base_path: work.base?.label || null, candidate_path: work.candidate?.label || null,
    base_source: work.base_source,
    comparison: work.base ? { rows: [{ kind: "equal", left: block(work.base), right: block(work.candidate) }], source_equal: false, removed: 0, added: 0 } : null,
    processing_ms: 0,
  });
  await page.addInitScript(() => {
    window.EventSource = class {
      static OPEN = 1;
      readyState = 1;
      listeners = {};
      constructor() { window.syntheticMemosEvents = this; queueMicrotask(() => { this.onopen?.(); this.listeners.revision?.(); }); }
      addEventListener(name, fn) { this.listeners[name] = (event = { lastEventId: "synthetic-session-a" }) => fn(event); }
      close() { this.readyState = 2; }
    };
  });
  await page.route("https://mddiff.example/**", async (route) => {
    const req = route.request();
    const url = new URL(req.url());
    const send = (data, status = 200) => route.fulfill({ status, contentType: "application/json", body: JSON.stringify(data) });
    if (assets[url.pathname]) return route.fulfill(assets[url.pathname]);
    if (url.pathname === "/auth/session") return send(authenticated ? { user_label: "Cuenta sintética", csrf_token: csrf } : {}, authenticated ? 200 : 401);
    if (url.pathname === "/auth/logout") { authenticated = false; return route.fulfill({ status: 204 }); }
    if (url.pathname === "/api/state") return send(snapshot());
    if (url.pathname === "/api/connections") return send({ enabled, destinations });
    if (url.pathname === "/api/work" && req.method() === "GET") return send(work);
    if (["/api/work", "/api/sources/memos"].includes(url.pathname)) {
      const body = req.postDataJSON();
      assert.equal(req.headers()["x-csrf-token"], csrf);
      assert.equal(req.headers().origin, "https://mddiff.example");
      if (url.pathname === "/api/sources/memos") {
        reads.push(body);
        if (readBarrier) { enteredRead?.(); await readBarrier; }
        if (result === "missing") return send({ error: "No se pudo leer la nota de Memos." }, 404);
        if (result === "timeout") return send({ error: "Memos tardó demasiado en responder. Vuelve a intentarlo." }, 504);
        if (body.expected_revision !== work.revision) return send({ error: "El trabajo cambió." }, 409);
        assert.deepEqual(Object.keys(body).sort(), ["destination", "expected_revision", "reference"]);
        assert.equal(body.destination, "notes");
        work.base = { label: "Memos · memos/abc123", markdown };
        work.base_source = { provider: "memos", destination: "notes", resource: "memos/abc123", url: "https://notes.example/memos/abc123", captured_at: 1793145600, updated_at: "2026-10-27T00:00:00Z" };
      } else {
        writes.push({ method: req.method(), body });
        if (body.expected_revision !== work.revision) return send({ error: "El trabajo cambió." }, 409);
        if (req.method() === "PATCH") {
          assert.deepEqual(Object.keys(body).sort(), ["candidate", "candidate_input", "expected_revision"]);
          work.candidate = body.candidate;
        } else {
          assert.equal(req.method(), "PUT");
          assert.equal(Object.hasOwn(body, "base_source"), false);
          work.base = body.base; work.candidate = body.candidate; work.base_source = null;
        }
      }
      work.revision++;
      return send(snapshot());
    }
    return route.fulfill({ status: 404, body: "" });
  });
  const open = async () => {
    await page.locator("#base-memos").click();
    await page.waitForFunction(() => !document.getElementById("memos-source-reference").disabled);
  };
  const submit = async (reference = "memos/abc123") => {
    await page.locator("#memos-source-reference").fill(reference);
    await page.locator("#memos-source-open").click();
  };
  const emitRevision = () => page.evaluate(() => window.syntheticMemosEvents.listeners.revision());
  await page.goto("https://mddiff.example/");
  await page.locator("#base-memos").waitFor({ state: "visible" });
  await page.waitForFunction(() => !document.getElementById("base-memos").disabled);

  phase = "explicit selection, exact content and source metadata";
  await open();
  assert.deepEqual(await page.locator("#memos-source-destination option").allTextContents(), ["Notas de prueba"]);
  assert.equal(await page.evaluate(() => document.activeElement.id), "memos-source-reference");
  await page.getByText("La nota reemplaza el original y conserva la propuesta actual. Se guarda una captura; no se sincroniza automáticamente.", { exact: true }).waitFor();
  assert.equal(reads.length, 0);
  const initialCandidate = structuredClone(work.candidate);
  await submit("https://notes.example/memos/abc123");
  await page.waitForFunction(() => !document.getElementById("memos-source-dialog").open);
  assert.equal(work.base.markdown, markdown);
  assert.deepEqual(work.candidate, initialCandidate);
  assert.equal(reads[0].expected_revision, 1);
  assert.equal(reads[0].reference, "https://notes.example/memos/abc123");
  assert.equal(await page.locator("#base-provenance a").getAttribute("href"), "https://notes.example/memos/abc123");
  assert.equal(await page.locator("#base-provenance a").getAttribute("rel"), "noopener noreferrer");
  assert.equal(await page.locator("#base-provenance .source-captured").getAttribute("datetime"), new Date(1793145600 * 1000).toISOString());
  assert.match(await page.locator("#base-provenance .source-captured").innerText(), /^Capturado /);
  await page.locator("#source-tab").click();
  assert.equal(await page.locator("#rows .cell code").first().textContent(), markdown);
  const readCount = reads.length;
  await page.waitForTimeout(450);
  assert.equal(reads.length, readCount, "A capture must not synchronize automatically");

  phase = "candidate paste and upload preserve provenance through PATCH";
  const provenance = structuredClone(work.base_source);
  await page.locator("#candidate-paste").click();
  await page.locator("#candidate-text").fill("# Nueva propuesta");
  assert.equal(await page.locator("#base-memos").isDisabled(), true);
  await page.getByText("Abierto y guardado.", { exact: true }).waitFor();
  await page.locator("#candidate-paste").click();
  assert.equal(writes.at(-1).method, "PATCH");
  assert.deepEqual(work.base_source, provenance);
  await page.locator("#candidate-upload").click();
  await page.locator("#candidate-file").setInputFiles({ name: "Nueva propuesta.md", mimeType: "text/markdown", buffer: Buffer.from("# Subida\r\nFinal") });
  await page.waitForFunction(() => document.getElementById("candidate-name").textContent === "Nueva propuesta.md");
  assert.equal(writes.at(-1).method, "PATCH");
  assert.deepEqual(work.base_source, provenance);
  assert.equal(work.candidate.markdown, "# Subida\r\nFinal");
  assert.equal(await page.locator("#base-provenance a").count(), 1);

  phase = "missing note and timeout preserve input and work";
  for (const outcome of ["missing", "timeout"]) {
    result = outcome;
    const before = structuredClone(work);
    await open();
    await submit("memos/unavailable");
    await page.locator("#memos-source-error").waitFor({ state: "visible" });
    assert.equal(await page.locator("#memos-source-reference").inputValue(), "memos/unavailable");
    assert.equal(await page.locator("#memos-source-open").isDisabled(), false);
    assert.equal(await page.locator("#memos-source-status").textContent(), "");
    assert.deepEqual(work, before);
    await page.keyboard.press("Escape");
    assert.equal(await page.evaluate(() => document.activeElement.id), "base-memos");
    assert.equal(await page.locator("#memos-source-reference").inputValue(), "");
  }
  result = "success";

  phase = "conflict uses the revision captured when opening";
  await open();
  const baseline = work.revision;
  work.revision++;
  work.candidate = { label: "Propuesta externa.md", markdown: "# Cambio externo" };
  await emitRevision();
  await page.waitForFunction(() => document.getElementById("candidate-name").textContent === "Propuesta externa.md");
  await submit();
  await page.locator("#memos-source-error").waitFor({ state: "visible" });
  assert.equal(reads.at(-1).expected_revision, baseline);
  assert.equal(await page.locator("#memos-source-reference").inputValue(), "memos/abc123");
  assert.equal(await page.locator("#memos-source-open").isDisabled(), true);
  assert.equal(work.candidate.label, "Propuesta externa.md");
  const failedReadCount = reads.length;
  await page.waitForTimeout(450);
  assert.equal(reads.length, failedReadCount);
  await page.locator("#memos-source-close").click();
  await open();
  await submit();
  await page.waitForFunction(() => !document.getElementById("memos-source-dialog").open);
  assert.equal(reads.at(-1).expected_revision, baseline + 1);

  phase = "provenance rejects unsafe links and clears on manual original";
  work.base_source.url = "javascript:alert(1)";
  work.revision++;
  await emitRevision();
  await page.waitForFunction(() => document.getElementById("base-provenance").querySelector("a") === null);
  assert.match(await page.locator("#base-provenance").textContent(), /Memos · memos\/abc123/);
  await page.locator("#base-paste").click();
  await page.locator("#base-text").fill("# Original manual");
  await page.getByText("Abierto y guardado.", { exact: true }).waitFor();
  await page.locator("#base-paste").click();
  assert.equal(writes.at(-1).method, "PUT");
  assert.equal(work.base_source, null);
  assert.equal(await page.locator("#base-provenance").isVisible(), false);

  phase = "empty connections and responsive native dialog";
  enabled = false;
  await page.locator("#base-memos").click();
  await page.getByText("No hay conexiones de Memos disponibles. Conecta tu cuenta desde Conexiones.", { exact: true }).waitFor();
  assert.equal(await page.locator("#memos-source-open").isDisabled(), true);
  await page.keyboard.press("Escape");
  enabled = true;
  await page.setViewportSize({ width: 390, height: 844 });
  await open();
  await page.locator("#memos-source-reference").fill("memos/abc123");
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth));
  await mkdir(new URL("target/browser/", root), { recursive: true });
  await page.screenshot({ path: fileURLToPath(new URL("target/browser/memos-mobile.png", root)) });
  await page.keyboard.press("Escape");
  await page.setViewportSize({ width: 1200, height: 850 });
  await open();
  await page.locator("#memos-source-reference").fill("memos/abc123");
  await page.screenshot({ path: fileURLToPath(new URL("target/browser/memos-desktop.png", root)) });
  await page.keyboard.press("Escape");

  phase = "logout clears input and ignores a late capture";
  await open();
  readBarrier = new Promise((resolve) => { releaseRead = resolve; });
  const started = new Promise((resolve) => { enteredRead = resolve; });
  await submit();
  await started;
  await page.locator("#memos-source-close").click();
  await page.locator("#logout-button").click();
  await page.locator("#login-button").waitFor({ state: "visible" });
  releaseRead();
  await page.waitForTimeout(100);
  assert.equal(await page.locator("#memos-source-reference").inputValue(), "");
  assert.equal(await page.locator("#memos-source-dialog").evaluate((node) => node.open), false);
  assert.equal(await page.locator("#base-provenance").textContent(), "");
  assert.equal(await page.locator("#rows").textContent(), "");

  phase = "account switch rejects a successful response from the previous session";
  authenticated = true;
  readBarrier = null;
  await page.goto("https://mddiff.example/");
  await page.waitForFunction(() => !document.getElementById("base-memos").disabled);
  await open();
  readBarrier = new Promise((resolve) => { releaseRead = resolve; });
  const secondStarted = new Promise((resolve) => { enteredRead = resolve; });
  await submit();
  await secondStarted;
  csrf = "synthetic-session-b";
  releaseRead();
  await page.locator("#login-button").waitFor({ state: "visible" });
  assert.equal(await page.locator("#memos-source-reference").inputValue(), "");
  assert.equal(await page.locator("#memos-source-destination option").count(), 0);
  assert.equal(await page.locator("#memos-source-dialog").evaluate((node) => node.open), false);
  assert.equal(await page.locator("#rows").textContent(), "");
  console.log("Memos UI: explicit read, exact content, source metadata, candidate PATCH, errors/timeouts, CAS, focus/mobile, logout and account changes passed.");
} catch (error) {
  throw new Error(`Memos UI failed during ${phase}: ${error.message}`);
} finally {
  await browser.close();
}
