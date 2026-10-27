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
  for (const id of ["reload-button", "example-button", "delete-work"]) {
    $(id).disabled = value;
    $(id).setAttribute("aria-busy", String(value));
  }
  $("reload-button").disabled = value || !$("base-path").value.trim();
  const pending = textDraft && !textDraft.conflict && textDraft.version !== textDraft.savedVersion && textDraft.version !== textDraft.failedVersion;
  for (const side of ["base", "candidate"]) {
    for (const control of ["paste", "upload", "file"]) {
      $(`${side}-${control}`).disabled = value || !canImport(side) || Boolean(pending && !(side === textDraft.side && control === "paste"));
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
  return request("/api/work", {
    expected_revision: work.revision,
    base: side === "base" ? document : work.base,
    candidate: side === "candidate" ? document : work.candidate,
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
  if (event.ctrlKey || event.metaKey || event.altKey || event.target.closest("input,textarea,[contenteditable=true]")) return;
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
    hostedView("Tu espacio privado", "Pega el original o sube un Markdown desde su cabecera para empezar.");
    $("document-status").textContent = "Solo tú puedes acceder a este espacio.";
    connectEvents();
  } catch (error) { showError(error); }
}
start();
