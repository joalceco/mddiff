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
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  let authenticated = true;
  let csrf = "source-session-a";
  let local = false;
  let outcome = "success";
  let remoteMarkdown = "# Jira inicial\n\nContenido convertido.";
  let barrier = null, entered, release;
  const work = { revision: 0, base: null, candidate: null, base_source: null };
  const metadata = { base_input: null, candidate_input: null };
  const imports = [];
  const writes = [];
  const captured = () => 1793232000 + work.revision;
  const destinations = [
    { id: "issues", provider: "jira", label: "Jira de prueba", url: "https://issues.example", connected: true, revision: 1 },
    { id: "other", provider: "jira", label: "Jira sin conectar", url: "https://other.example", connected: false, revision: 0 },
    { id: "notes", provider: "memos", label: "Memos de prueba", url: "https://notes.example", connected: true, revision: 1 },
  ];
  const block = (d) => d && ({ source: d.markdown, html: "<p>Markdown convertido</p>", start_line: 1, end_line: 3 });
  const snapshot = () => ({
    revision: work.revision, base_path: work.base?.label || null, candidate_path: work.candidate?.label || null,
    base_source: work.base_source, ...metadata,
    comparison: work.base ? { rows: [{ kind: "modified", left: block(work.base), right: block(work.candidate) }], source_equal: false, removed: 1, added: 1 } : null,
    processing_ms: 0,
  });
  await page.addInitScript(() => {
    window.EventSource = class {
      static OPEN = 1;
      readyState = 1;
      listeners = {};
      constructor() { window.sourceEvents = this; queueMicrotask(() => { this.onopen?.(); this.listeners.revision?.(); }); }
      addEventListener(name, fn) { this.listeners[name] = (event = { lastEventId: "source-session-a" }) => fn(event); }
      close() { this.readyState = 2; }
    };
  });
  await page.route("https://mddiff.example/**", async (route) => {
    const req = route.request(), url = new URL(req.url());
    const send = (data, status = 200) => route.fulfill({ status, contentType: "application/json", body: JSON.stringify(data) });
    if (assets[url.pathname]) return route.fulfill(assets[url.pathname]);
    if (url.pathname === "/auth/session") return send(authenticated ? { user_label: "Usuario sintético", csrf_token: csrf } : {}, local ? 404 : authenticated ? 200 : 401);
    if (url.pathname === "/auth/logout") { authenticated = false; return route.fulfill({ status: 204 }); }
    if (url.pathname === "/api/state") return send(snapshot());
    if (url.pathname === "/api/connections") return send({ enabled: true, destinations });
    if (url.pathname === "/api/work" && req.method() === "GET") return send(work);
    if (["/api/work", "/api/sources/jira"].includes(url.pathname)) {
      const body = req.postDataJSON();
      assert.equal(req.headers()["x-csrf-token"], csrf);
      assert.equal(req.headers().origin, "https://mddiff.example");
      if (url.pathname === "/api/sources/jira") {
        imports.push(body);
        assert.deepEqual(Object.keys(body).sort(), ["destination", "expected_revision", "reference"]);
        assert.equal(body.destination, "issues");
        if (barrier) { entered?.(); await barrier; }
        if (outcome === "missing") return send({ error: "El issue no está disponible para esta conexión." }, 404);
        if (outcome === "denied") return send({ error: "La conexión no tiene permiso para leer este issue." }, 502);
        if (body.expected_revision !== work.revision) return send({ error: "El trabajo cambió." }, 409);
        work.base = { label: "PROJ-123 · Título de Jira", markdown: remoteMarkdown };
        work.base_source = { provider: "jira", destination: "issues", resource: "PROJ-123", title: "Título de Jira <script>", url: "https://issues.example/browse/PROJ-123", captured_at: captured(), updated_at: "2026-10-29T01:00:00.000Z", converter: "adf-md-v1", warnings: ["Nodo no soportado: mystery. Se conserva su JSON."] };
        metadata.base_input = null;
      } else {
        writes.push({ method: req.method(), body });
        if (body.expected_revision !== work.revision) return send({ error: "El trabajo cambió." }, 409);
        if (req.method() === "PATCH") {
          assert.ok(["text", "file"].includes(body.candidate_input));
          work.candidate = body.candidate;
          metadata.candidate_input = { kind: body.candidate_input, captured_at: captured() };
        } else {
          assert.equal(req.method(), "PUT");
          assert.equal(body.base_input, "text");
          work.base = body.base; work.candidate = body.candidate; work.base_source = null;
          metadata.base_input = { kind: body.base_input, captured_at: captured() };
        }
      }
      work.revision++;
      return send(snapshot());
    }
    return route.fulfill({ status: 404, body: "" });
  });
  const emit = () => page.evaluate(() => window.sourceEvents.listeners.revision());
  const ready = () => page.waitForFunction(() => !document.getElementById("base-jira").disabled);
  await page.goto("https://mddiff.example/");
  await ready();
  assert.equal(await page.getByRole("group", { name: "Elegir origen del original" }).getByRole("button").count(), 4);
  assert.equal(await page.locator("#candidate-agent").isDisabled(), true);

  phase = "manual capture metadata comes from the server and survives reconnect";
  await page.locator("#base-paste").click();
  await page.locator("#base-text").fill("# Original manual");
  await page.getByText("Abierto y guardado.", { exact: true }).waitFor();
  await page.locator("#base-paste").click();
  assert.equal(writes[0].body.base_input, "text");
  assert.match(await page.locator("#base-provenance").innerText(), /Texto pegado/);
  assert.equal(await page.locator("#base-provenance .source-received").getAttribute("datetime"), new Date(metadata.base_input.captured_at * 1000).toISOString());
  await page.reload();
  await ready();
  assert.match(await page.locator("#base-provenance").innerText(), /Texto pegado/);
  await page.locator("#candidate-paste").click();
  await page.locator("#candidate-text").fill("# Propuesta pegada");
  await page.getByText("Abierto y guardado.", { exact: true }).waitFor();
  await page.locator("#candidate-paste").click();
  assert.equal(writes.at(-1).body.candidate_input, "text");
  await page.locator("#candidate-upload").click();
  await page.locator("#candidate-file").setInputFiles({ name: "propuesta.md", mimeType: "text/markdown", buffer: Buffer.from("# Propuesta de archivo") });
  await page.waitForFunction(() => document.getElementById("candidate-name").textContent === "propuesta.md");
  assert.equal(writes.at(-1).body.candidate_input, "file");
  assert.match(await page.locator("#candidate-provenance").innerText(), /Archivo Markdown/);

  phase = "keyboard source selection, title, reference, timestamp and visible conversion warnings";
  await page.locator("#base-jira").focus();
  await page.keyboard.press("Enter");
  await page.getByRole("dialog", { name: "Abrir Jira Cloud" }).waitFor();
  await page.waitForFunction(() => !document.getElementById("memos-source-reference").disabled);
  assert.equal(await page.evaluate(() => document.activeElement.id), "memos-source-reference");
  assert.deepEqual(await page.locator("#memos-source-destination option").allTextContents(), ["Jira de prueba"]);
  await page.locator("#memos-source-reference").fill("https://issues.example/browse/PROJ-123");
  await page.keyboard.press("Enter");
  await page.waitForFunction(() => !document.getElementById("memos-source-dialog").open);
  assert.equal(imports[0].expected_revision, 3);
  assert.equal(await page.locator("#base-provenance a").getAttribute("href"), "https://issues.example/browse/PROJ-123");
  assert.equal(await page.locator("#base-provenance .source-title").textContent(), "Título de Jira <script>");
  assert.equal(await page.locator("#base-provenance script").count(), 0);
  assert.match(await page.locator("#base-provenance").innerText(), /Conversión adf-md-v1/);
  assert.equal(await page.locator("#base-provenance .source-updated").getAttribute("datetime"), "2026-10-29T01:00:00.000Z");
  await page.getByText("Nodo no soportado: mystery. Se conserva su JSON.", { exact: true }).waitFor();
  const candidate = structuredClone(work.candidate);
  const frozen = work.base.markdown;
  const count = imports.length;
  remoteMarkdown = "# Jira cambió de forma remota";
  await emit();
  await page.waitForTimeout(150);
  assert.equal(imports.length, count);
  assert.equal(work.base.markdown, frozen);

  phase = "explicit refresh preserves proposal and failures preserve the captured original";
  outcome = "denied";
  await page.locator("#base-jira").click();
  await page.waitForFunction(() => !document.getElementById("memos-source-reference").disabled);
  await page.locator("#memos-source-reference").fill("PROJ-403");
  await page.locator("#memos-source-open").click();
  await page.getByText("La conexión no tiene permiso para leer este issue.", { exact: true }).waitFor();
  assert.equal(await page.locator("#memos-source-reference").inputValue(), "PROJ-403");
  assert.equal(await page.locator("#auth-user").isVisible(), true);
  assert.equal(work.base.markdown, frozen);
  await page.keyboard.press("Escape");
  for (const [failure, message] of [["missing", "El issue no está disponible para esta conexión."], ["denied", "La conexión no tiene permiso para leer este issue."]]) {
    outcome = failure;
    await page.locator("#base-refresh-source").click();
    await page.getByText(message, { exact: true }).first().waitFor();
    assert.equal(work.base.markdown, frozen);
    assert.deepEqual(work.candidate, candidate);
    assert.equal(await page.locator("#auth-user").isVisible(), true);
  }
  outcome = "success";
  await page.locator("#base-refresh-source").click();
  await page.getByText("Captura actualizada. La propuesta se conserva.", { exact: true }).waitFor();
  assert.equal(work.base.markdown, remoteMarkdown);
  assert.deepEqual(work.candidate, candidate);
  assert.equal(imports.at(-1).reference, "PROJ-123");

  phase = "late refresh rejects the old revision rather than rebasing automatically";
  barrier = new Promise((resolve) => { release = resolve; });
  const started = new Promise((resolve) => { entered = resolve; });
  const expectedRevision = work.revision;
  await page.locator("#base-refresh-source").click();
  await started;
  work.candidate = { label: "agente.md", markdown: "# Propuesta nueva del agente" };
  metadata.candidate_input = { kind: "agent", captured_at: captured() };
  work.revision++;
  await emit();
  await page.waitForFunction(() => document.getElementById("candidate-name").textContent === "agente.md");
  release();
  await page.getByText("El trabajo cambió durante la lectura. Se conserva el trabajo reciente; vuelve a pulsar Actualizar origen para reintentar.", { exact: true }).first().waitFor();
  assert.equal(imports.at(-1).expected_revision, expectedRevision);
  assert.equal(work.candidate.label, "agente.md");
  assert.match(await page.locator("#candidate-provenance").innerText(), /Agente · MCP/);
  barrier = null;
  const afterConflict = imports.length;
  await page.waitForTimeout(200);
  assert.equal(imports.length, afterConflict);

  phase = "agent instructions do not change content; compact controls remain usable on mobile";
  await page.locator("#candidate-agent").click();
  await page.getByText("Recibe la propuesta en este espacio", { exact: true }).waitFor();
  assert.equal(await page.locator("#candidate-agent-url").textContent(), "https://mddiff.example/mcp");
  assert.equal(imports.length, afterConflict);
  await page.setViewportSize({ width: 390, height: 844 });
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
  assert.ok(await page.locator("#document-view").evaluate((node) => node.getBoundingClientRect().height > 100), "Source details must leave room for the document on mobile");
  await mkdir(new URL("target/browser/", root), { recursive: true });
  await page.screenshot({ path: fileURLToPath(new URL("target/browser/sources-mobile.png", root)) });
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.locator("#candidate-agent").click();
  await page.screenshot({ path: fileURLToPath(new URL("target/browser/sources-desktop.png", root)) });

  phase = "late capture from another session is never rendered";
  barrier = new Promise((resolve) => { release = resolve; });
  const finalStarted = new Promise((resolve) => { entered = resolve; });
  await page.locator("#base-refresh-source").click();
  await finalStarted;
  csrf = "source-session-b";
  release();
  await page.locator("#login-button").waitFor({ state: "visible" });
  assert.equal(await page.locator("#base-provenance").textContent(), "");
  assert.equal(await page.locator("#candidate-provenance").textContent(), "");
  assert.equal(await page.locator("#candidate-agent-url").textContent(), "");
  assert.equal(await page.locator("#rows").textContent(), "");

  phase = "events from another or missing session never trigger a refresh";
  barrier = null;
  csrf = "source-session-a";
  for (const id of ["", "source-session-b"]) {
    await page.reload();
    await ready();
    await page.evaluate((lastEventId) => window.sourceEvents.listeners.revision({ lastEventId }), id);
    await page.locator("#login-button").waitFor({ state: "visible" });
    assert.equal(await page.locator("#rows").textContent(), "");
    assert.equal(await page.locator("#base-provenance").textContent(), "");
  }

  phase = "local reader still uses paths and exposes no hosted source controls";
  barrier = null;
  local = true;
  await page.reload();
  await page.waitForFunction(() => document.getElementById("connection").textContent === "Conectado");
  assert.equal(await page.locator("#base-path").isVisible(), true);
  assert.equal(await page.locator("#base-source-actions").isVisible(), false);
  assert.equal(await page.locator("#candidate-source-actions").isVisible(), false);
  assert.equal(await page.locator("#base-refresh-source").isVisible(), false);
  assert.deepEqual(errors, []);
  console.log("Sources UI: manual metadata, Jira/Memos choices, conversion warnings, explicit refresh/CAS, agent delivery, account changes, keyboard/mobile and local reader passed.");
} catch (error) {
  throw new Error(`Sources UI failed during ${phase}: ${error.message}`);
} finally {
  await browser.close();
}
