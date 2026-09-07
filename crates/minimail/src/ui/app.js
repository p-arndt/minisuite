// minimail web UI. Vanilla ES2020, no build step. Consumes the §8 JSON API + SSE.
'use strict';

const API = '/api/v1';
const $ = (id) => document.getElementById(id);

const ui = {
  app: $('app'),
  list: $('msg-list'),
  listEmpty: $('list-empty'),
  emptyHint: $('empty-hint'),
  search: $('search'),
  conn: $('conn'),
  connLabel: ui_qs('.conn-label'),
  addrs: $('addrs'),
  clearAll: $('clear-all'),
  previewEmpty: $('preview-empty'),
  preview: $('preview'),
  subject: $('pv-subject'),
  from: $('pv-from'),
  to: $('pv-to'),
  ccRow: $('pv-cc-row'),
  cc: $('pv-cc'),
  date: $('pv-date'),
  deleteOne: $('delete-one'),
  tabs: $('tabs'),
  tabBody: $('tab-body'),
  attCount: $('att-count'),
};
function ui_qs(sel) { return document.querySelector(sel); }

// ---- state ----
let messages = [];        // newest first, each a Summary object
let selectedId = null;
let detail = null;        // full message detail of the selected id
let activeTab = 'html';
let searching = false;    // debounce guard for list reloads

// ---- helpers ----
const ICON_CLIP = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21.4 11.05 12.25 20.2a5 5 0 0 1-7.07-7.07l9.19-9.19a3.33 3.33 0 0 1 4.71 4.71l-9.2 9.19a1.67 1.67 0 0 1-2.36-2.36l8.49-8.48"/></svg>';
const ICON_FILE = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round"><path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M14 3v5h5"/></svg>';
const ICON_DL = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12m0 0 4-4m-4 4-4-4M5 21h14"/></svg>';

// Set trusted static markup only (never message-derived).
function icon(svg, cls) { const s = document.createElement('span'); if (cls) s.className = cls; s.innerHTML = svg; return s; }

function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text != null) n.textContent = text;
  return n;
}

function relTime(unix) {
  const s = Math.floor(Date.now() / 1000) - unix;
  if (s < 45) return 'now';
  if (s < 3600) return Math.floor(s / 60) + 'm';
  if (s < 86400) return Math.floor(s / 3600) + 'h';
  if (s < 604800) return Math.floor(s / 86400) + 'd';
  return new Date(unix * 1000).toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
}

function humanSize(n) {
  if (n < 1024) return n + ' B';
  if (n < 1048576) return (n / 1024).toFixed(1) + ' KB';
  return (n / 1048576).toFixed(1) + ' MB';
}

function senderOf(s) { return s.from_header || s.from || '(unknown sender)'; }
function subjectOf(s) { return s.subject || ''; }

async function api(path, opts) {
  const r = await fetch(API + path, Object.assign({ headers: { Accept: 'application/json' } }, opts));
  if (!r.ok) throw new Error('HTTP ' + r.status);
  return r;
}

// ---- info bar ----
async function loadInfo() {
  try {
    const info = await (await api('/info')).json();
    ui.addrs.replaceChildren();
    ui.addrs.append(el('b', null, 'SMTP'), document.createTextNode(' ' + info.smtp + '  '),
                     el('b', null, 'HTTP'), document.createTextNode(' ' + info.http));
    ui.emptyHint.textContent = 'Send mail to ' + info.smtp + ' and it appears here instantly.';
  } catch (e) { /* leave defaults */ }
}

// ---- list ----
async function loadList() {
  const q = ui.search.value.trim();
  const path = '/messages?limit=500' + (q ? '&q=' + encodeURIComponent(q) : '');
  try {
    const data = await (await api(path)).json();
    messages = data.messages || [];
    renderList();
  } catch (e) { /* keep current view */ }
}

function renderList() {
  ui.list.replaceChildren();
  for (const s of messages) ui.list.append(rowFor(s));
  ui.listEmpty.hidden = messages.length > 0;
  if (selectedId && !messages.some((m) => m.id === selectedId)) clearPreview();
}

function rowFor(s) {
  const li = el('li', 'msg');
  li.dataset.id = s.id;
  li.dataset.unix = s.received_unix;
  if (s.id === selectedId) li.classList.add('sel');

  const r1 = el('div', 'msg-row1');
  r1.append(el('span', 'msg-from', senderOf(s)), el('span', 'msg-time', relTime(s.received_unix)));

  const r2 = el('div', 'msg-row2');
  const subj = subjectOf(s);
  r2.append(el('span', 'msg-subject' + (subj ? '' : ' none'), subj || '(no subject)'));
  if (s.attachments && s.attachments.length) r2.append(icon(ICON_CLIP, 'msg-clip'));

  li.append(r1, r2);
  li.addEventListener('click', () => select(s.id));
  return li;
}

function updateTimes() {
  for (const li of ui.list.children) {
    const t = li.querySelector('.msg-time');
    if (t) t.textContent = relTime(Number(li.dataset.unix));
  }
}

// ---- selection + preview ----
async function select(id) {
  selectedId = id;
  for (const li of ui.list.children) li.classList.toggle('sel', li.dataset.id === id);
  try {
    detail = await (await api('/messages/' + encodeURIComponent(id))).json();
  } catch (e) { return; }
  renderPreview();
}

function clearPreview() {
  selectedId = null;
  detail = null;
  ui.preview.hidden = true;
  ui.previewEmpty.hidden = false;
}

function renderPreview() {
  const s = detail.summary;
  ui.previewEmpty.hidden = true;
  ui.preview.hidden = false;

  const subj = s.subject || '';
  ui.subject.textContent = subj || '(no subject)';
  ui.subject.classList.toggle('none', !subj);
  ui.from.textContent = s.from_header || s.from || '(unknown)';
  ui.to.textContent = s.to_header || (s.to || []).join(', ') || '(none)';
  if (s.cc_header) { ui.cc.textContent = s.cc_header; ui.ccRow.hidden = false; } else { ui.ccRow.hidden = true; }
  ui.date.textContent = s.date_header || new Date(s.received_unix * 1000).toLocaleString();

  const atts = s.attachments || [];
  ui.attCount.hidden = atts.length === 0;
  ui.attCount.textContent = atts.length;

  // pick a sensible default tab if the current one has no content
  if ((activeTab === 'html' && !s.has_html) || (activeTab === 'text' && !s.has_text)) {
    activeTab = s.has_html ? 'html' : s.has_text ? 'text' : 'raw';
  }
  showTab(activeTab);
}

function showTab(name) {
  activeTab = name;
  for (const b of ui.tabs.querySelectorAll('.tab')) b.classList.toggle('active', b.dataset.tab === name);
  ui.tabBody.replaceChildren(renderTabBody(name));
}

function renderTabBody(name) {
  const s = detail.summary;
  const id = s.id;
  if (name === 'html') {
    if (!detail.html) return el('div', 'tab-empty', 'This message has no HTML part.');
    // Security boundary: untrusted mail is rendered in a sandboxed iframe with
    // NO allow-scripts, so message scripts cannot run or reach the app.
    const f = document.createElement('iframe');
    f.className = 'mail-frame';
    f.setAttribute('sandbox', '');
    f.setAttribute('referrerpolicy', 'no-referrer');
    f.srcdoc = detail.html;
    return f;
  }
  if (name === 'text') {
    if (detail.text == null) return el('div', 'tab-empty', 'This message has no plain-text part.');
    return el('pre', 'plain', detail.text);
  }
  if (name === 'raw') {
    const wrap = el('div');
    const actions = el('div', 'raw-actions');
    const a = el('a', 'link', 'Download .eml');
    a.href = API + '/messages/' + encodeURIComponent(id) + '/raw';
    actions.append(icon(ICON_DL), a);
    const pre = el('pre', 'raw', 'Loading…');
    wrap.append(actions, pre);
    fetch(API + '/messages/' + encodeURIComponent(id) + '/raw')
      .then((r) => r.text())
      .then((t) => { if (activeTab === 'raw') pre.textContent = t; })
      .catch(() => { pre.textContent = '(failed to load raw source)'; });
    return wrap;
  }
  if (name === 'headers') {
    const table = el('table', 'headers-table');
    const tb = el('tbody');
    for (const [k, v] of (detail.headers || [])) {
      const tr = el('tr');
      tr.append(el('td', 'hk', k), el('td', 'hv', v));
      tb.append(tr);
    }
    table.append(tb);
    return table;
  }
  if (name === 'attachments') {
    const atts = s.attachments || [];
    if (!atts.length) return el('div', 'tab-empty', 'No attachments.');
    const ul = el('ul', 'att-list');
    for (const a of atts) {
      const li = document.createElement('a');
      li.className = 'att';
      li.href = API + '/messages/' + encodeURIComponent(id) + '/parts/' +
                encodeURIComponent(a.part_id) + '?download=1';
      const main = el('div', 'att-main');
      main.append(el('div', 'att-name', a.filename || '(unnamed)'),
                  el('div', 'att-meta', (a.content_type || 'application/octet-stream') + ' · ' + humanSize(a.size)));
      li.append(icon(ICON_FILE, 'att-icon'), main, icon(ICON_DL, 'att-dl'));
      ul.append(li);
    }
    return ul;
  }
  return el('div');
}

// ---- mutations ----
async function deleteOne(id) {
  try { await api('/messages/' + encodeURIComponent(id), { method: 'DELETE' }); } catch (e) { return; }
  removeMessage(id); // optimistic; SSE delete is idempotent
}

function removeMessage(id) {
  messages = messages.filter((m) => m.id !== id);
  if (selectedId === id) clearPreview();
  renderList();
}

function addMessage(summary) {
  if (ui.search.value.trim()) { scheduleReconcile(); return; } // let the server filter
  messages = messages.filter((m) => m.id !== summary.id);
  messages.unshift(summary);
  renderList();
}

let reconcileTimer = null;
function scheduleReconcile() {
  clearTimeout(reconcileTimer);
  reconcileTimer = setTimeout(loadList, 250);
}

async function clearAll() {
  if (!confirm('Delete all captured messages? This cannot be undone.')) return;
  try { await api('/messages', { method: 'DELETE' }); } catch (e) { return; }
  messages = [];
  clearPreview();
  renderList();
}

// ---- live updates: SSE with polling fallback ----
let es = null;
let pollTimer = null;

function setConn(live) {
  ui.conn.classList.toggle('live', live);
  ui.connLabel.textContent = live ? 'live' : 'offline';
}

function startSSE() {
  if (typeof EventSource === 'undefined') { startPolling(); return; }
  try {
    es = new EventSource(API + '/events');
  } catch (e) { startPolling(); return; }
  es.addEventListener('open', () => { stopPolling(); setConn(true); loadList(); });
  es.addEventListener('message', (e) => { try { addMessage(JSON.parse(e.data)); } catch (_) {} });
  es.addEventListener('delete', (e) => { try { removeMessage(JSON.parse(e.data).id); } catch (_) {} });
  es.addEventListener('clear', () => { messages = []; clearPreview(); renderList(); });
  es.addEventListener('error', () => {
    setConn(false);
    // EventSource auto-reconnects while CONNECTING; only fall back once it gives up.
    if (es.readyState === EventSource.CLOSED) { es = null; startPolling(); }
  });
}

function startPolling() {
  if (pollTimer) return;
  setConn(false);
  loadList();
  pollTimer = setInterval(loadList, 4000);
  // keep trying to restore the live stream
  if (typeof EventSource !== 'undefined' && !es) setTimeout(startSSE, 15000);
}

function stopPolling() {
  if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
}

// ---- keyboard navigation ----
function moveSelection(delta) {
  if (!messages.length) return;
  let idx = messages.findIndex((m) => m.id === selectedId);
  idx = idx < 0 ? (delta > 0 ? 0 : messages.length - 1) : idx + delta;
  idx = Math.max(0, Math.min(messages.length - 1, idx));
  const target = messages[idx];
  select(target.id);
  const li = ui.list.querySelector('.msg[data-id="' + CSS.escape(target.id) + '"]');
  if (li) li.scrollIntoView({ block: 'nearest' });
}

document.addEventListener('keydown', (e) => {
  const t = e.target;
  if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA')) {
    if (e.key === 'Escape') t.blur();
    return;
  }
  if (e.key === 'j' || e.key === 'ArrowDown') { e.preventDefault(); moveSelection(1); }
  else if (e.key === 'k' || e.key === 'ArrowUp') { e.preventDefault(); moveSelection(-1); }
  else if (e.key === '/') { e.preventDefault(); ui.search.focus(); }
});

// ---- wiring ----
let searchTimer = null;
ui.search.addEventListener('input', () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(loadList, 180);
});
ui.clearAll.addEventListener('click', clearAll);
ui.deleteOne.addEventListener('click', () => { if (selectedId) deleteOne(selectedId); });
ui.tabs.addEventListener('click', (e) => {
  const b = e.target.closest('.tab');
  if (b) showTab(b.dataset.tab);
});

setInterval(updateTimes, 30000);

// ---- boot ----
loadInfo();
loadList();
startSSE();
