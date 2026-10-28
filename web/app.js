"use strict";

const $ = (id) => document.getElementById(id);
function applyTheme(theme) {
  document.documentElement.dataset.theme = theme;
  $("theme-toggle").textContent = theme === "dark" ? "Modo claro" : "Modo oscuro";
}
try { applyTheme(localStorage.getItem("mddiff-theme") === "light" ? "light" : "dark"); }
catch { applyTheme("dark"); }
$("theme-toggle").addEventListener("click", () => {
  const theme = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
  applyTheme(theme);
  try { localStorage.setItem("mddiff-theme", theme); } catch {}
});

let session = null;
let acceptedRevision = -1;
let view = "reading";
let format = "rendered";
let changes = [];
let changeIndex = -1;
let busy = false;
let openTimer = null;
let inputVersion = 0;
let pathsDirty = false;
let authentication = null;
let hosted = false;
let events = null;
let reconnectTimer = null;
let textDraft = null;
let importVersion = 0;
const fileRevisions = { base: null, candidate: null };
const MAX_DOCUMENT_BYTES = 2 * 1024 * 1024;
let connectionsVersion = 0;
let connectionsBusy = false;
let memosDestination = null;
let memosSourceVersion = 0;
let memosSourceBusy = false;
let memosSourceRevision = null;
let memosSourceConflict = false;

function activeMemosSource(previous, version) {
  return Boolean(previous && authentication === previous && memosSourceVersion === version && $("memos-source-dialog").open);
}

function setMemosSourceBusy(value) {
  memosSourceBusy = value;
  $("memos-source-dialog").setAttribute("aria-busy", String(value));
  const unavailable = value || !$("memos-source-destination").options.length;
  $("memos-source-destination").disabled = $("memos-source-reference").disabled = unavailable;
  $("memos-source-open").disabled = unavailable || memosSourceConflict;
  setBusy(busy);
}

function closeMemosSource() {
  memosSourceVersion++;
  memosSourceRevision = null;
  memosSourceConflict = false;
  $("memos-source-reference").value = "";
  $("memos-source-destination").replaceChildren();
  $("memos-source-status").textContent = "";
  $("memos-source-error").textContent = "";
  $("memos-source-error").hidden = true;
  setMemosSourceBusy(false);
  if ($("memos-source-dialog").open) $("memos-source-dialog").close();
}

async function memosSourceRequest(path, body, method) {
  const previous = authentication;
  const version = memosSourceVersion;
  const data = await request(path, body, method);
  if (!activeMemosSource(previous, version)) return null;
  if (!await sameWebSession(previous)) {
    if (activeMemosSource(previous, version)) endSession();
    return null;
  }
  return activeMemosSource(previous, version) ? data : null;
}

function memosSourceError(error) {
  $("memos-source-status").textContent = "";
  $("memos-source-error").textContent = error.status === 409
    ? "El trabajo cambió. Cierra y vuelve a abrir este diálogo para reintentar con el trabajo actual."
    : error instanceof TypeError ? "No se pudo abrir Memos. Comprueba la conexión y vuelve a intentarlo." : error.message || "No se pudo abrir la nota. Vuelve a intentarlo.";
  $("memos-source-error").hidden = false;
}

function pendingTextImport() {
  return Boolean(textDraft && textDraft.version !== textDraft.savedVersion);
}

async function openMemosSource() {
  if (busy || memosSourceBusy || pendingTextImport() || !canImport("base")) return;
  closeConnections();
  closeMemosSource();
  memosSourceRevision = session.revision;
  const version = memosSourceVersion;
  const previous = authentication;
  $("memos-source-dialog").showModal();
  $("memos-source-status").textContent = "Cargando conexiones…";
  setMemosSourceBusy(true);
  try {
    const data = await memosSourceRequest("/api/connections");
    if (!data) return;
    const destinations = data.enabled ? (data.destinations || []).filter((item) => item.provider === "memos" && item.connected) : [];
    for (const destination of destinations) {
      const option = document.createElement("option");
      option.value = destination.id; option.textContent = destination.label;
      $("memos-source-destination").append(option);
    }
    $("memos-source-status").textContent = destinations.length ? "Solo se lee el contenido de la nota, sin adjuntos." : "No hay conexiones de Memos disponibles. Conecta tu cuenta desde Conexiones.";
  } catch (error) { if (activeMemosSource(previous, version)) memosSourceError(error); }
  finally {
    if (activeMemosSource(previous, version)) {
      setMemosSourceBusy(false);
      if ($("memos-source-destination").options.length) $("memos-source-reference").focus();
    }
  }
}

$("base-memos").addEventListener("click", openMemosSource);
$("memos-source-close").addEventListener("click", closeMemosSource);
$("memos-source-dialog").addEventListener("cancel", (event) => { event.preventDefault(); closeMemosSource(); });
$("memos-source-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (memosSourceBusy || memosSourceConflict || !authentication || !$("memos-source-dialog").open || !$("memos-source-destination").value) return;
  const version = memosSourceVersion;
  const previous = authentication;
  const reference = $("memos-source-reference").value.trim();
  if (!reference) { $("memos-source-reference").focus(); return; }
  $("memos-source-error").hidden = true;
  $("memos-source-status").textContent = "Leyendo y guardando la captura…";
  setMemosSourceBusy(true);
  try {
    const snapshot = await memosSourceRequest("/api/sources/memos", {
      destination: $("memos-source-destination").value, reference, expected_revision: memosSourceRevision,
    }, "POST");
    if (!snapshot) return;
    resetImports();
    applySession(snapshot);
    closeMemosSource();
  } catch (error) {
    if (activeMemosSource(previous, version)) {
      memosSourceConflict = error.status === 409;
      memosSourceError(error);
    }
  } finally { if (activeMemosSource(previous, version)) setMemosSourceBusy(false); }
});

function clearMemosForm() {
  memosDestination = null;
  $("memos-token").value = "";
  $("memos-acknowledge").checked = false;
  $("memos-form").hidden = true;
}

function closeConnections() {
  connectionsVersion++;
  clearMemosForm();
  $("connections-list").replaceChildren();
  $("connections-status").textContent = "";
  $("connections-error").textContent = "";
  $("connections-error").hidden = true;
  setConnectionsBusy(false);
  if ($("connections-dialog").open) $("connections-dialog").close();
}

function setConnectionsBusy(value) {
  connectionsBusy = value;
  $("connections-dialog").setAttribute("aria-busy", String(value));
  for (const node of $("connections-dialog").querySelectorAll("[data-connection-action], #memos-token, #memos-acknowledge, #memos-submit, #memos-cancel")) node.disabled = value;
}

function activeConnections(previous, version) {
  return Boolean(previous && authentication === previous && connectionsVersion === version && $("connections-dialog").open);
}

async function connectionsRequest(path, body, method) {
  const previous = authentication;
  const version = connectionsVersion;
  const data = await request(path, body, method);
  if (!activeConnections(previous, version)) return null;
  if (!await sameWebSession(previous)) {
    if (activeConnections(previous, version)) endSession();
    return null;
  }
  return activeConnections(previous, version) ? data : null;
}

function connectionError(error) {
  $("connections-status").textContent = "";
  $("connections-error").textContent = error.message || "No se pudo actualizar la conexión. Vuelve a intentarlo.";
  $("connections-error").hidden = false;
}

async function loadConnections() {
  const data = await connectionsRequest("/api/connections");
  if (!data) return false;
  const connectionDestinations = data.enabled ? data.destinations || [] : [];
  const list = document.createDocumentFragment();
  for (const destination of connectionDestinations) {
    const row = document.createElement("li");
    row.dataset.destination = destination.id;
    const details = document.createElement("div");
    details.className = "connection-details";
    const title = document.createElement("strong"); title.textContent = destination.label;
    const address = document.createElement("span"); address.className = "connection-address"; address.textContent = `${destination.provider === "jira" ? "Jira" : "Memos"} · ${destination.url}`;
    const state = document.createElement("span"); state.className = "connection-state"; state.textContent = destination.connected ? "Conectada" : "Sin conectar";
    details.append(title, address, state);
    const actions = document.createElement("div"); actions.className = "connection-actions";
    const entries = [[destination.connected ? "Reconectar" : "Conectar", "connect"]];
    if (destination.connected) entries.push(["Comprobar", "check"], ["Desconectar", "disconnect"]);
    for (const [label, action] of entries) {
      const button = document.createElement("button"); button.type = "button"; button.textContent = label;
      button.dataset.connectionAction = action;
      button.setAttribute("aria-label", `${label} ${destination.label}`);
      button.addEventListener("click", () => connectionAction(destination, action));
      actions.append(button);
    }
    row.append(details, actions); list.append(row);
  }
  $("connections-list").replaceChildren(list);
  $("connections-status").textContent = !data.enabled ? "Las conexiones no están habilitadas en este servidor." : connectionDestinations.length ? "Cada conexión pertenece únicamente a tu cuenta." : "No hay destinos aprobados disponibles. Consulta al administrador.";
  return true;
}

async function openConnections(message = "") {
  if (!hosted || !authentication) return;
  closeConnections();
  const version = connectionsVersion;
  const previous = authentication;
  $("connections-dialog").showModal();
  $("connections-status").textContent = "Cargando destinos…";
  setConnectionsBusy(true);
  try {
    if (await loadConnections() && message) $("connections-status").textContent = message;
  } catch (error) { if (activeConnections(previous, version)) connectionError(error); }
  finally { if (activeConnections(previous, version)) setConnectionsBusy(false); }
}

async function connectionAction(destination, action) {
  if (connectionsBusy || !authentication || !$("connections-dialog").open) return;
  clearMemosForm();
  $("connections-error").hidden = true;
  if (action === "connect" && destination.provider === "memos") {
    memosDestination = destination;
    $("memos-title").textContent = `Conectar Memos · ${destination.label}`;
    $("memos-form").hidden = false;
    $("memos-token").focus();
    return;
  }
  const version = connectionsVersion;
  const previous = authentication;
  const path = `/api/connections/${encodeURIComponent(destination.id)}`;
  setConnectionsBusy(true);
  $("connections-status").textContent = action === "connect" ? "Abriendo autorización de Jira…" : action === "check" ? "Comprobando conexión…" : "Desconectando…";
  try {
    const data = await connectionsRequest(action === "connect" ? `${path}/authorize` : action === "check" ? `${path}/check` : path, { expected_revision: destination.revision }, action === "disconnect" ? "DELETE" : "POST");
    if (!data) return;
    if (action === "connect") {
      const url = new URL(data.authorization_url);
      if (url.protocol !== "https:" || url.username || url.password) throw new Error("La autorización de Jira no está disponible. Vuelve a intentarlo.");
      window.location.assign(url.href);
    } else if (await loadConnections()) {
      $("connections-status").textContent = action === "check" ? "Conexión comprobada." : "Conexión eliminada de mddiff. Tu trabajo se conserva.";
    }
  } catch (error) {
    if (activeConnections(previous, version)) {
      if (error.status === 409) await loadConnections().catch(() => {});
      if (activeConnections(previous, version)) connectionError(error);
    }
  } finally { if (activeConnections(previous, version)) setConnectionsBusy(false); }
}

$("connections-button").addEventListener("click", () => openConnections());
$("connections-close").addEventListener("click", closeConnections);
$("connections-dialog").addEventListener("cancel", (event) => { event.preventDefault(); closeConnections(); });
$("connections-dialog").addEventListener("close", () => { if (!$("connections-dialog").open) clearMemosForm(); });
$("memos-cancel").addEventListener("click", clearMemosForm);
$("memos-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (connectionsBusy || !memosDestination || !authentication || !$("connections-dialog").open || !$("memos-acknowledge").checked) return;
  const destination = memosDestination;
  const token = $("memos-token").value;
  if (!token.trim()) { $("memos-token").focus(); return; }
  const version = connectionsVersion;
  const previous = authentication;
  $("memos-token").value = "";
  $("memos-acknowledge").checked = false;
  $("connections-error").hidden = true;
  setConnectionsBusy(true);
  $("connections-status").textContent = "Guardando conexión…";
  try {
    const data = await connectionsRequest(`/api/connections/${encodeURIComponent(destination.id)}/memos`, { expected_revision: destination.revision, token, acknowledge_full_access: true }, "POST");
    if (!data) return;
    clearMemosForm();
    if (await loadConnections()) $("connections-status").textContent = "Conexión guardada. mddiff solo realiza lecturas.";
  } catch (error) {
    if (activeConnections(previous, version)) {
      if (error.status === 409) { clearMemosForm(); await loadConnections().catch(() => {}); }
      if (activeConnections(previous, version)) connectionError(error);
    }
  } finally { if (activeConnections(previous, version)) setConnectionsBusy(false); }
});

async function request(path, body, method = "POST") {
  const response = await fetch(path, body === undefined ? {} : {
    method, headers: { "Content-Type": "application/json", ...(authentication ? { "X-CSRF-Token": authentication.csrf_token } : {}) }, body: JSON.stringify(body),
  });
  const text = await response.text();
  let data;
  try { data = JSON.parse(text); } catch { data = null; }
  if (!response.ok) {
    if (hosted && [401, 403].includes(response.status)) endSession();
    const error = new Error(data?.error || data?.message || text || `Error ${response.status}`);
    error.status = response.status;
    throw error;
  }
  return data;
}

function showError(error) {
  $("error").textContent = error.message || String(error);
  $("error").hidden = false;
}

function setBusy(value) {
  busy = value;
  const working = value || memosSourceBusy;
  for (const id of ["reload-button", "example-button", "delete-work"]) {
    $(id).disabled = working;
    $(id).setAttribute("aria-busy", String(working));
  }
  $("reload-button").disabled = working || !$("base-path").value.trim();
  $("base-memos").disabled = working || !canImport("base") || pendingTextImport();
  const pending = textDraft && !textDraft.conflict && textDraft.version !== textDraft.savedVersion && textDraft.version !== textDraft.failedVersion;
  for (const side of ["base", "candidate"]) {
    for (const control of ["paste", "upload", "file"]) {
      $(`${side}-${control}`).disabled = working || !canImport(side) || Boolean(pending && !(side === textDraft.side && control === "paste"));
    }
  }
}

function canImport(side) {
  return Boolean(hosted && authentication && session && (side === "base" || session.base_path));
}

function importStatus(side, message, invalid = false) {
  const node = $(`${side}-import-status`);
  node.textContent = message;
  node.classList.toggle("invalid", invalid);
}

function resetImports() {
  importVersion++;
  clearTimeout(textDraft?.timer);
  textDraft = null;
  for (const side of ["base", "candidate"]) {
    fileRevisions[side] = null;
    $(`${side}-text`).value = "";
    $(`${side}-text`).hidden = true;
    $(`${side}-file`).value = "";
    $(`${side}-paste`).textContent = "Pegar texto";
    $(`${side}-paste`).setAttribute("aria-label", side === "base" ? "Pegar texto del original" : "Pegar texto de la propuesta");
    $(`${side}-paste`).setAttribute("aria-expanded", "false");
    importStatus(side, "Markdown UTF-8 · máx. 2 MiB");
  }
  setBusy(busy);
}

function importError(side, error) {
  importStatus(side, error.message || String(error), true);
  showError(error);
}

async function replaceSide(work, side, document) {
  if (document.markdown.length > MAX_DOCUMENT_BYTES || new TextEncoder().encode(document.markdown).length > MAX_DOCUMENT_BYTES) {
    throw new Error("Cada documento admite hasta 2 MiB en UTF-8.");
  }
  if (side === "candidate" && !work.base) throw new Error("Abre primero el original.");
  if (side === "candidate") return request("/api/work", { expected_revision: work.revision, candidate: document }, "PATCH");
  return request("/api/work", {
    expected_revision: work.revision,
    base: document,
    candidate: work.candidate,
  }, "PUT");
}

async function openText(side) {
  if (busy || !canImport(side)) return;
  if (textDraft?.side === side) {
    const draft = textDraft;
    if (!draft.conflict && draft.version !== draft.savedVersion && draft.version !== draft.failedVersion) {
      clearTimeout(draft.timer);
      await saveText(draft);
      if (textDraft !== draft || draft.version !== draft.savedVersion) return;
    }
    resetImports();
    return;
  }
  resetImports();
  const version = importVersion;
  setBusy(true);
  try {
    const work = await request("/api/work");
    if (!authentication || version !== importVersion) return;
    textDraft = { side, work, value: "", version: 0, savedVersion: 0, failedVersion: -1, conflict: false };
    const field = $(`${side}-text`);
    field.hidden = false;
    $(`${side}-paste`).textContent = "Cerrar texto";
    $(`${side}-paste`).setAttribute("aria-label", side === "base" ? "Cerrar texto del original" : "Cerrar texto de la propuesta");
    $(`${side}-paste`).setAttribute("aria-expanded", "true");
    importStatus(side, "Se abre automáticamente al dejar de escribir.");
    field.focus();
  } catch (error) { if (version === importVersion && authentication) importError(side, error); }
  finally { setBusy(false); }
}

function queueText(draft) {
  clearTimeout(draft.timer);
  draft.timer = setTimeout(() => saveText(draft), 350);
  setBusy(busy);
}

async function saveText(draft) {
  if (textDraft !== draft || !authentication || busy || draft.conflict
    || draft.version === draft.savedVersion || draft.version === draft.failedVersion) return;
  const version = draft.version;
  const document = { label: draft.side === "base" ? "Original pegado.md" : "Propuesta pegada.md", markdown: draft.value };
  setBusy(true);
  importStatus(draft.side, "Abriendo y guardando…");
  try {
    const snapshot = await replaceSide(draft.work, draft.side, document);
    if (textDraft !== draft || !authentication) return;
    // Advance only our own revision; incoming SSE must not rebase an unsaved draft.
    draft.work = { ...draft.work, [draft.side]: document, revision: snapshot.revision };
    draft.savedVersion = version;
    applySession(snapshot);
    importStatus(draft.side, draft.version === version ? "Abierto y guardado." : "Pendiente de abrir…");
  } catch (error) {
    if (textDraft !== draft || !authentication) return;
    draft.failedVersion = version;
    if (error.status === 409) {
      draft.conflict = true;
      await refresh();
      error.message = "El trabajo cambió. Conserva tu texto y cierra y vuelve a abrir este campo antes de reintentar.";
    }
    importError(draft.side, error);
  } finally {
    setBusy(false);
    if (textDraft === draft && draft.version !== version && !draft.conflict) queueText(draft);
  }
}

async function openUpload(side) {
  const input = $(`${side}-file`);
  const file = input.files[0];
  input.value = "";
  if (!file || busy || !canImport(side)) return;
  const expected = fileRevisions[side] ?? session.revision;
  fileRevisions[side] = null;
  resetImports();
  const version = importVersion;
  setBusy(true);
  importStatus(side, "Leyendo Markdown…");
  try {
    if (!/\.(md|markdown)$/i.test(file.name)) throw new Error("Elige un archivo .md o .markdown.");
    if (file.size > MAX_DOCUMENT_BYTES) throw new Error("Cada documento admite hasta 2 MiB.");
    const label = fileName(file.name);
    if (!label.trim() || new TextEncoder().encode(label).length > 255 || /[\u0000-\u001f\u007f-\u009f]/.test(label)) {
      throw new Error("El nombre requiere entre 1 y 255 bytes, sin caracteres de control.");
    }
    const work = await request("/api/work");
    if (!authentication || version !== importVersion) return;
    if (work.revision !== expected) {
      const error = new Error("El trabajo cambió mientras elegías el archivo. Vuelve a seleccionarlo para reintentar.");
      error.status = 409;
      throw error;
    }
    let bytes;
    try { bytes = await file.arrayBuffer(); }
    catch { throw new Error("No se pudo leer el archivo. Vuelve a seleccionarlo."); }
    let markdown;
    try {
      // Keep BOM and line endings; reject malformed UTF-8 instead of replacing bytes.
      markdown = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes);
    } catch { throw new Error("El archivo no contiene texto UTF-8 válido."); }
    if (!authentication || version !== importVersion) return;
    const snapshot = await replaceSide(work, side, { label, markdown });
    if (!authentication || version !== importVersion) return;
    applySession(snapshot);
    importStatus(side, "Abierto y guardado.");
  } catch (error) {
    if (version !== importVersion || !authentication) return;
    if (error.status === 409) await refresh();
    importError(side, error);
  } finally { setBusy(false); }
}

for (const side of ["base", "candidate"]) {
  $(`${side}-paste`).addEventListener("click", () => openText(side));
  $(`${side}-upload`).addEventListener("click", () => {
    if (busy || !canImport(side)) return;
    fileRevisions[side] = session.revision;
    $(`${side}-file`).click();
  });
  $(`${side}-file`).addEventListener("change", () => openUpload(side));
  const field = $(`${side}-text`);
  const changed = (value) => {
    if (textDraft?.side !== side) return;
    textDraft.value = value;
    textDraft.version++;
    if (!textDraft.conflict) {
      importStatus(side, "Pendiente de abrir…");
      queueText(textDraft);
    }
  };
  field.addEventListener("input", (event) => { if (!event.isComposing) changed(field.value); });
  field.addEventListener("compositionend", () => changed(field.value));
  field.addEventListener("paste", (event) => {
    // A complete paste preserves clipboard CRLF even though textarea.value normalizes it.
    if (event.clipboardData?.types.includes("text/plain") && field.selectionStart === 0 && field.selectionEnd === field.value.length) {
      event.preventDefault();
      const value = event.clipboardData.getData("text/plain");
      field.value = value;
      changed(value);
    }
  });
}

async function openFiles(example = false) {
  if (hosted) return;
  clearTimeout(openTimer);
  openTimer = null;
  if (busy) return;
  if (example) {
    $("base-path").value = "examples/base.md";
    $("candidate-path").value = "examples/propuesta.md";
    pathsDirty = true;
    inputVersion++;
  }
  const base = $("base-path").value.trim();
  const candidate = $("candidate-path").value.trim();
  if (!base) { pathsDirty = false; return; }
  const version = inputVersion;
  const compare = Boolean(candidate);
  setBusy(true);
  $("error").hidden = true;
  try {
    const data = await request(compare ? "/api/compare" : "/api/open", compare
      ? { base_path: base, candidate_path: candidate } : { path: base });
    if (data?.comparison) applySession(data);
    else await refresh();
  } catch (error) { showError(error); }
  finally {
    setBusy(false);
    if (inputVersion === version) pathsDirty = false;
    else if (!openTimer) openFiles();
  }
}

function applySession(data) {
  if (!data || data.revision <= acceptedRevision) return;
  acceptedRevision = data.revision;
  session = data;
  view = data.candidate_path ? "diff" : "reading";
  if (!pathsDirty) {
    $("base-path").value = data.base_path || "";
    $("candidate-path").value = data.candidate_path || "";
  }
  $("error").hidden = true;
  $("document-scroll").scrollTop = 0;
  render();
}

async function refresh() {
  try {
    const data = await request("/api/state");
    if (!hosted || authentication) applySession(data);
  }
  catch (error) { showError(error); }
}

function fileName(path) { return path?.split(/[\\/]/).pop() || ""; }

function renderBaseSource() {
  const node = $("base-provenance");
  const source = hosted && authentication && session?.base_source;
  node.replaceChildren();
  node.hidden = !source;
  if (!source) return;
  const label = `Memos · ${source.resource}`;
  let url;
  try {
    url = new URL(source.url);
    const local = ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname) && url.origin === window.location.origin;
    if ((url.protocol !== "https:" && !(url.protocol === "http:" && local)) || url.username || url.password) url = null;
  } catch { url = null; }
  const origin = document.createElement(url ? "a" : "span");
  origin.textContent = label;
  if (url) { origin.href = url.href; origin.target = "_blank"; origin.rel = "noopener noreferrer"; }
  node.append(origin);
  const captured = new Date(source.captured_at * 1000);
  if (Number.isFinite(source.captured_at) && !Number.isNaN(captured.valueOf())) {
    const time = document.createElement("time");
    time.dateTime = captured.toISOString();
    time.textContent = `Capturado ${captured.toLocaleString("es", { dateStyle: "medium", timeStyle: "short" })}`;
    node.append(time);
  }
}

function cell(block, status) {
  const node = document.createElement("div");
  node.className = `cell ${status}`;
  if (!block) { node.classList.add("placeholder"); node.setAttribute("aria-hidden", "true"); return node; }
  if (status) {
    const sign = document.createElement("span");
    sign.className = "change-sign";
    sign.textContent = status === "added" ? "+" : "−";
    sign.setAttribute("aria-label", status === "added" ? "Bloque añadido" : "Bloque eliminado");
    node.append(sign);
  }
  const content = document.createElement("div");
  content.className = "cell-content";
  if (format === "source") {
    const label = document.createElement("span");
    label.className = "line-label";
    label.textContent = `L${block.start_line}–${block.end_line}`;
    const pre = document.createElement("pre");
    const code = document.createElement("code");
    if (status && block.highlighted_source) {
      for (const part of block.highlighted_source) {
        if (!part.changed) { code.append(document.createTextNode(part.text)); continue; }
        const mark = document.createElement("mark");
        mark.className = "diff-change";
        mark.textContent = part.text;
        code.append(mark);
      }
    } else code.textContent = block.source;
    pre.append(code);
    content.append(label, pre);
  } else {
    // Rust sanitizes both HTML variants; source stays in text nodes.
    content.innerHTML = status && block.highlighted_html ? block.highlighted_html : block.html;
    for (const link of content.querySelectorAll("a")) {
      link.target = "_blank";
      link.rel = "noopener noreferrer";
    }
    if (!block.html.trim() && status) {
      const note = document.createElement("span");
      note.className = "source-note";
      note.textContent = "Espacios o referencias · consulta Fuente";
      content.append(note);
    }
  }
  node.append(content);
  return node;
}

function render() {
  const comparison = session?.comparison;
  const hasDocument = Boolean(comparison);
  const isDiff = hasDocument && view === "diff" && Boolean(session.candidate_path);
  $("empty").hidden = hasDocument;
  $("review-bar").hidden = !hasDocument;
  $("document-view").hidden = !hasDocument;
  $("delete-work").hidden = !hosted || !authentication || !hasDocument;
  if (hosted) {
    $("pane-headers").hidden = !authentication;
    $("base-path").hidden = $("candidate-path").hidden = true;
  }
  for (const side of ["base", "candidate"]) {
    $(`${side}-source-actions`).hidden = $(`${side}-import-status`).hidden = !hosted || !authentication;
  }
  $("document-view").classList.toggle("reading", !isDiff);
  $("rows").classList.toggle("source", format === "source");
  setBusy(busy);
  $("diff-tab").disabled = !session?.candidate_path;
  for (const [id, selected] of [["reading-tab", !isDiff], ["diff-tab", isDiff], ["rendered-tab", format === "rendered"], ["source-tab", format === "source"]]) {
    $(id).classList.toggle(["reading-tab", "diff-tab"].includes(id) ? "active" : "selected", selected);
    $(id).setAttribute("aria-pressed", String(selected));
  }
  $("base-name").textContent = (hosted ? session?.base_path : fileName(session?.base_path)) || "Sin archivo";
  $("candidate-name").textContent = (hosted ? session?.candidate_path : fileName(session?.candidate_path)) || "Sin archivo";
  renderBaseSource();
  if (!hasDocument) {
    changes = [];
    changeIndex = -1;
    $("rows").replaceChildren();
    $("review-summary").replaceChildren();
    $("change-badge").hidden = true;
    if (hosted && authentication) {
      $("document-status").textContent = "Solo tú puedes acceder a este espacio.";
      $("timing").textContent = "Espacio privado";
    }
    return;
  }
  const fragment = document.createDocumentFragment();
  let rowGroup;
  changes = [];
  changeIndex = -1;
  for (const row of comparison.rows) {
    if (!isDiff && !row.left) continue;
    const node = document.createElement("div");
    node.className = "diff-row";
    const changed = row.kind !== "equal";
    node.append(cell(row.left, isDiff && changed && row.left ? "removed" : ""));
    if (isDiff) node.append(cell(row.right, changed && row.right ? "added" : ""));
    if (isDiff && changed) { node.tabIndex = -1; changes.push(node); }
    if (!rowGroup || rowGroup.childElementCount === 64) {
      rowGroup = document.createElement("div");
      rowGroup.className = "row-group";
      fragment.append(rowGroup);
    }
    rowGroup.append(node);
  }
  const visibleRows = fragment.childElementCount;
  $("rows").replaceChildren(fragment);
  $("change-badge").hidden = !session.candidate_path;
  $("change-badge").textContent = String(comparison.rows.filter((r) => r.kind !== "equal").length);
  $("review-summary").replaceChildren();
  if (isDiff) {
    if (comparison.source_equal) $("review-summary").textContent = "Sin cambios · los archivos son idénticos";
    else {
      const removed = document.createElement("span"); removed.className = "stat-removed"; removed.textContent = `−${comparison.removed}`;
      const added = document.createElement("span"); added.className = "stat-added"; added.textContent = `+${comparison.added}`;
      $("review-summary").append(removed, added, document.createTextNode("  bloques · original → propuesta"));
    }
  } else $("review-summary").textContent = "Modo lectura · documento original";
  $("change-navigation").hidden = !isDiff || changes.length === 0;
  updatePosition();
  $("document-status").textContent = isDiff ? "Dos versiones · desplazamiento alineado · J / K para navegar" : "Solo lectura · los archivos no se modifican";
  $("timing").textContent = `${Number(session.processing_ms || 0).toFixed(1)} ms en Rust`;
  if (visibleRows === 0) {
    const empty = document.createElement("p"); empty.className = "cell source-note"; empty.textContent = "Documento vacío."; $("rows").append(empty);
  }
}

function updatePosition() { $("change-position").textContent = `${changeIndex + 1} / ${changes.length}`; }
function navigateChange(delta) {
  if (!changes.length) return;
  if (changeIndex >= 0) changes[changeIndex].classList.remove("current");
  changeIndex = changeIndex < 0 ? (delta > 0 ? 0 : changes.length - 1) : (changeIndex + delta + changes.length) % changes.length;
  const target = changes[changeIndex];
  target.classList.add("current");
  target.scrollIntoView({ block: "center" });
  target.focus({ preventScroll: true });
  updatePosition();
}

for (const id of ["base-path", "candidate-path"]) {
  $(id).addEventListener("input", () => {
    pathsDirty = true;
    inputVersion++;
    clearTimeout(openTimer);
    openTimer = setTimeout(() => openFiles(), 350);
    setBusy(busy);
  });
  $(id).addEventListener("keydown", (event) => {
    if (event.key === "Enter") { event.preventDefault(); openFiles(); }
  });
}
$("reload-button").addEventListener("click", () => openFiles());
$("example-button").addEventListener("click", () => openFiles(true));
$("delete-work").addEventListener("click", async () => {
  if (!hosted || !authentication || busy || !session?.comparison) return;
  const expectedRevision = session.revision;
  if (!window.confirm("¿Borrar el original y la propuesta actuales?\nEsta acción vacía tu trabajo. No se puede deshacer desde mddiff.")) return;
  resetImports();
  setBusy(true);
  try { applySession(await request("/api/work", { expected_revision: expectedRevision }, "DELETE")); }
  catch (error) {
    if (error.status === 409) await refresh();
    showError(error);
  }
  finally { setBusy(false); }
});
$("reading-tab").addEventListener("click", () => { if (view === "reading") return; view = "reading"; render(); });
$("diff-tab").addEventListener("click", () => { if (view === "diff") return; view = "diff"; render(); });
$("rendered-tab").addEventListener("click", () => { if (format === "rendered") return; format = "rendered"; render(); });
$("source-tab").addEventListener("click", () => { if (format === "source") return; format = "source"; render(); });
$("previous-change").addEventListener("click", () => navigateChange(-1));
$("next-change").addEventListener("click", () => navigateChange(1));
document.addEventListener("keydown", (event) => {
  if ($("connections-dialog").open || $("memos-source-dialog").open || event.ctrlKey || event.metaKey || event.altKey || event.target.closest("input,select,textarea,[contenteditable=true]")) return;
  if (event.key === "j" || event.key === "k") { event.preventDefault(); navigateChange(event.key === "j" ? 1 : -1); }
});

function hostedView(title, description) {
  $("pane-headers").hidden = !authentication;
  $("reload-button").hidden = true;
  $("example-button").hidden = true;
  $("empty").querySelector("ol").hidden = true;
  $("empty").querySelector("h1").textContent = title;
  $("empty").querySelector("p").textContent = description;
  $("empty").querySelector(".small-label").textContent = "MDDIFF";
  $("timing").textContent = "Espacio privado";
}

function endSession() {
  authentication = null;
  closeConnections();
  closeMemosSource();
  resetImports();
  clearTimeout(reconnectTimer);
  events?.close();
  clearTimeout(openTimer);
  session = null;
  acceptedRevision = -1;
  changes = [];
  $("rows").replaceChildren();
  $("base-path").value = $("candidate-path").value = "";
  $("auth-user").textContent = "";
  $("auth-user").hidden = $("logout-button").hidden = true;
  $("connections-button").hidden = true;
  $("login-button").hidden = false;
  $("connection").textContent = "Sin sesión";
  $("connection").classList.remove("connected");
  $("document-status").textContent = "Inicia sesión para continuar.";
  $("error").hidden = true;
  render();
  hostedView("Sesión finalizada", "Vuelve a iniciar sesión para acceder a tus documentos.");
}

async function sameWebSession(previous) {
  try {
    const response = await fetch("/auth/session");
    const current = response.ok ? await response.json() : null;
    return Boolean(previous && current && current.csrf_token === previous.csrf_token);
  } catch { return false; }
}

function connectEvents() {
  const source = new EventSource("/api/events");
  events = source;
  let ready = Promise.resolve(false);
  source.onopen = () => {
    ready = (async () => {
      const previous = authentication;
      if (hosted) {
        const valid = await sameWebSession(previous);
        if (events !== source || source.readyState !== EventSource.OPEN || authentication !== previous) return false;
        if (!valid) { endSession(); return false; }
      }
      acceptedRevision = -1;
      $("connection").textContent = "Conectado";
      $("connection").classList.add("connected");
      return true;
    })();
  };
  source.addEventListener("revision", async () => {
    if (await ready && events === source && source.readyState === EventSource.OPEN && (!hosted || authentication)) refresh();
  });
  source.onerror = async () => {
    if (events !== source) return;
    ready = Promise.resolve(false);
    $("connection").textContent = "Reconectando…";
    $("connection").classList.remove("connected");
    if (hosted) {
      source.close();
      const previous = authentication;
      const valid = await sameWebSession(previous);
      if (events !== source || authentication !== previous) return;
      if (!valid) endSession();
      else reconnectTimer = setTimeout(() => { if (authentication === previous && events === source) connectEvents(); }, 3000);
    }
  };
}

$("login-button").addEventListener("click", () => { window.location.assign("/auth/login"); });
$("logout-button").addEventListener("click", async () => {
  closeConnections();
  closeMemosSource();
  $("logout-button").disabled = true;
  try {
    await request("/auth/logout", {});
    endSession();
  } catch (error) { showError(error); }
  finally { $("logout-button").disabled = false; }
});
window.addEventListener("pageshow", (event) => { if (event.persisted) window.location.reload(); });

async function start() {
  try {
    const response = await fetch("/auth/session");
    if (response.status === 404) { connectEvents(); return; }
    hosted = true;
    if (!response.ok) { endSession(); return; }
    authentication = await response.json();
    $("auth-user").textContent = authentication.user_label;
    $("auth-user").hidden = $("logout-button").hidden = false;
    $("connections-button").hidden = false;
    hostedView("Tu espacio privado", "Pega el original o sube un Markdown desde su cabecera para empezar.");
    $("document-status").textContent = "Solo tú puedes acceder a este espacio.";
    connectEvents();
    const result = new URL(window.location.href).searchParams.get("connection");
    if (["connected", "error"].includes(result)) {
      const url = new URL(window.location.href); url.searchParams.delete("connection");
      history.replaceState(null, "", url.pathname + url.search + url.hash);
      await openConnections(result === "connected" ? "Conexión completada." : "No se pudo conectar Jira. Vuelve a intentarlo.");
    }
  } catch (error) { showError(error); }
}
start();
