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
  let writes = 0;
  let failWrite = false;
  let delayedWrite = null;
  let releaseWrite;
  const destinations = [
    { id: "notes", provider: "memos", label: "Notas de prueba", url: "https://notes.example", connected: false, revision: 0 },
    { id: "issues", provider: "jira", label: "Jira de prueba", url: "https://issues.example", connected: false, revision: 0 },
  ];
  const mutations = [];
  await page.addInitScript(() => {
    window.EventSource = class {
      static OPEN = 1;
      readyState = 1;
      listeners = {};
      constructor() { window.syntheticConnectionEvents = this; queueMicrotask(() => { this.onopen?.(); this.listeners.revision?.(); }); }
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
    if (url.pathname === "/api/state") return send({ revision: 0, base_path: null, candidate_path: null, comparison: null, processing_ms: 0 });
    if (url.pathname === "/api/connections") return send({ enabled: true, destinations });
    const parts = url.pathname.split("/");
    const destination = destinations.find((item) => item.id === parts[3]);
    if (destination && url.pathname.startsWith("/api/connections/")) {
      const body = req.postDataJSON();
      mutations.push({ method: req.method(), path: url.pathname, body });
      assert.equal(req.headers()["x-csrf-token"], "synthetic-session-a");
      assert.equal(req.headers().origin, "https://mddiff.example");
      if (parts[4] === "memos") {
        writes++;
        if (delayedWrite) { releaseWrite?.(); await delayedWrite; }
        if (failWrite) return send({ error: "No se pudo guardar la conexión. Vuelve a intentarlo." }, 400);
        assert.equal(body.acknowledge_full_access, true);
        destination.connected = true;
      } else if (parts[4] === "authorize") {
        return send({ authorization_url: "https://mddiff.example/provider-authorization" });
      } else if (req.method() === "DELETE") destination.connected = false;
      destination.revision++;
      return send({ revision: destination.revision, connected: destination.connected });
    }
    if (url.pathname === "/provider-authorization") return route.fulfill({ contentType: "text/html", body: "<p>Autorización sintética</p>" });
    return route.fulfill({ status: 404, body: "" });
  });
  await page.goto("https://mddiff.example/");
  await page.locator("#connections-button").waitFor({ state: "visible" });

  phase = "dialog focus and explicit Memos acknowledgement";
  await page.locator("#connections-button").click();
  await page.getByRole("button", { name: "Conectar Notas de prueba", exact: true }).click();
  assert.equal(await page.locator("#memos-token").getAttribute("type"), "password");
  assert.equal(await page.locator("#memos-token").getAttribute("autocomplete"), "off");
  await page.getByText("El token de Memos puede permitir cambios. mddiff solo realiza lecturas.", { exact: true }).waitFor();
  await page.locator("#memos-token").fill("synthetic-memos-token");
  await page.locator("#memos-submit").click();
  assert.equal(writes, 0, "Unchecked acknowledgement sent a token");
  await page.locator("#memos-acknowledge").check();
  await page.locator("#memos-submit").click();
  await page.getByRole("button", { name: "Desconectar Notas de prueba", exact: true }).waitFor();
  assert.equal(await page.locator("#memos-token").inputValue(), "");
  assert.equal(await page.evaluate(() => JSON.stringify(localStorage).includes("synthetic-memos-token")), false);

  phase = "check, disconnect and cancellation";
  await page.getByRole("button", { name: "Comprobar Notas de prueba", exact: true }).click();
  await page.getByText("Conexión comprobada.", { exact: true }).waitFor();
  await page.getByRole("button", { name: "Desconectar Notas de prueba", exact: true }).click();
  await page.getByRole("button", { name: "Conectar Notas de prueba", exact: true }).waitFor();
  assert.ok(mutations.some((item) => item.method === "DELETE" && item.body.expected_revision === 2));
  await page.getByRole("button", { name: "Conectar Notas de prueba", exact: true }).click();
  await page.locator("#memos-token").fill("synthetic-cancelled-token");
  await page.locator("#memos-cancel").click();
  assert.equal(await page.locator("#memos-token").inputValue(), "");
  assert.equal(writes, 1);
  await page.keyboard.press("Escape");
  await page.waitForFunction(() => !document.getElementById("connections-dialog").open);
  assert.equal(await page.evaluate(() => document.activeElement.id), "connections-button");

  phase = "safe errors and mobile layout";
  await page.setViewportSize({ width: 390, height: 844 });
  await page.locator("#connections-button").click();
  await page.getByRole("button", { name: "Conectar Notas de prueba", exact: true }).click();
  await page.locator("#memos-token").fill("synthetic-rejected-token");
  await page.locator("#memos-acknowledge").check();
  failWrite = true;
  await page.locator("#memos-submit").click();
  await page.locator("#connections-error").waitFor({ state: "visible" });
  assert.equal(await page.locator("#memos-token").inputValue(), "");
  assert.equal(await page.locator("#connections-error").innerText(), "No se pudo guardar la conexión. Vuelve a intentarlo.");
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth));
  await mkdir(new URL("target/browser/", root), { recursive: true });
  await page.screenshot({ path: fileURLToPath(new URL("target/browser/connections-mobile.png", root)) });
  await page.setViewportSize({ width: 1200, height: 850 });
  await page.screenshot({ path: fileURLToPath(new URL("target/browser/connections-desktop.png", root)) });
  await page.locator("#connections-close").click();
  failWrite = false;

  phase = "Jira uses the same tab";
  await page.locator("#connections-button").click();
  await page.getByRole("button", { name: "Conectar Jira de prueba", exact: true }).click();
  await page.waitForURL("https://mddiff.example/provider-authorization");
  await page.goto("https://mddiff.example/?connection=error");
  await page.getByText("No se pudo conectar Jira. Vuelve a intentarlo.", { exact: true }).waitFor();
  assert.equal(new URL(page.url()).search, "");
  await page.locator("#connections-close").click();

  phase = "logout clears secrets and late responses stay hidden";
  await page.locator("#connections-button").click();
  await page.getByRole("button", { name: "Conectar Notas de prueba", exact: true }).click();
  await page.locator("#memos-token").fill("synthetic-late-token");
  await page.locator("#memos-acknowledge").check();
  let finishWrite;
  delayedWrite = new Promise((resolve) => { finishWrite = resolve; });
  const entered = new Promise((resolve) => { releaseWrite = resolve; });
  await page.locator("#memos-submit").click();
  await entered;
  await page.locator("#connections-close").click();
  await page.locator("#logout-button").click();
  await page.locator("#login-button").waitFor({ state: "visible" });
  finishWrite();
  await page.waitForTimeout(100);
  assert.equal(await page.locator("#memos-token").inputValue(), "");
  assert.equal(await page.locator("#connections-dialog").evaluate((node) => node.open), false);
  assert.equal(await page.locator("#connections-list").textContent(), "");
  assert.equal(await page.locator("#connections-button").isVisible(), false);

  phase = "account changes clear the dialog and draft";
  authenticated = true;
  delayedWrite = null;
  await page.goto("https://mddiff.example/");
  await page.locator("#connections-button").click();
  await page.locator('[data-destination="notes"] [data-connection-action="connect"]').click();
  await page.locator("#memos-token").fill("synthetic-account-switch-token");
  csrf = "synthetic-session-b";
  await page.evaluate(() => window.syntheticConnectionEvents.onerror());
  await page.locator("#login-button").waitFor({ state: "visible" });
  assert.equal(await page.locator("#memos-token").inputValue(), "");
  assert.equal(await page.locator("#memos-acknowledge").isChecked(), false);
  assert.equal(await page.locator("#connections-dialog").evaluate((node) => node.open), false);
  assert.equal(await page.locator("#connections-list").textContent(), "");
  console.log("Connections UI: acknowledgement, private token handling, cancel/focus, check/disconnect, errors, Jira return, mobile, logout races and account changes passed.");
} catch {
  throw new Error(`Connections UI failed during ${phase}; sensitive values were not logged.`);
} finally {
  await browser.close();
}
