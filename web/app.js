// Ransible web UI: dependency-free SPA talking to /api/v2/.
(() => {
'use strict';
const $app = document.getElementById('app');
let token = localStorage.getItem('rs_token') || '';
let me = null;
let stream = null, poll = null;

// ---------- helpers ----------
function h(tag, attrs, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (k === 'class') el.className = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (v !== false && v != null) el.setAttribute(k, v === true ? '' : v);
  }
  for (const k of kids.flat()) if (k != null && k !== false) el.append(k.nodeType ? k : document.createTextNode(String(k)));
  return el;
}
async function api(path, opts = {}) {
  const res = await fetch('/api/v2' + path, {
    method: opts.method || 'GET',
    headers: { 'Content-Type': 'application/json', ...(token ? { Authorization: 'Bearer ' + token } : {}) },
    body: opts.body ? JSON.stringify(opts.body) : undefined,
  });
  if (res.status === 401 && !path.startsWith('/auth/login')) { logout(true); throw new Error('Session expired'); }
  if (res.status === 204) return null;
  const ct = res.headers.get('content-type') || '';
  const data = ct.includes('json') ? await res.json() : await res.text();
  if (!res.ok) throw new Error((data && data.detail) || res.statusText);
  return data;
}
function toast(msg, err) {
  const t = h('div', { class: 'toast' + (err ? ' err' : '') }, msg);
  document.body.append(t); setTimeout(() => t.remove(), err ? 6000 : 2800);
}
const guard = (fn) => async (...a) => { try { await fn(...a); } catch (e) { toast(e.message, true); } };
const badge = (s) => h('span', { class: 'badge b-' + s }, s);
const when = (s) => (s ? s.replace('T', ' ').slice(0, 19) : '—');
const can = (lvl) => me && (me.role === 'admin' || (lvl === 'operate' && me.role === 'operator') || lvl === 'read');
function ansi(text) {
  const frag = document.createDocumentFragment();
  const re = /\x1b\[(\d+)m/g; let last = 0, cls = null, m;
  const push = (s) => { if (!s) return; const n = document.createTextNode(s); if (cls) { const sp = h('span', { class: cls }); sp.append(n); frag.append(sp); } else frag.append(n); };
  while ((m = re.exec(text))) { push(text.slice(last, m.index)); cls = m[1] === '0' ? null : 'a' + m[1]; last = re.lastIndex; }
  push(text.slice(last)); return frag;
}
function modal(title, body, actions) {
  const bg = h('div', { class: 'modal-bg', onclick: (e) => e.target === bg && bg.remove() });
  const close = () => bg.remove();
  bg.append(h('div', { class: 'modal', role: 'dialog', 'aria-label': title }, h('h2', {}, title), body, h('div', { class: 'foot' }, actions(close))));
  document.body.append(bg); const f = bg.querySelector('input,select,textarea'); if (f) f.focus(); return close;
}
function form(fields, values) {
  const inputs = {};
  const el = h('div', {});
  let row = null;
  fields.forEach((f, i) => {
    let input;
    const v = values && values[f.name] != null ? values[f.name] : f.default;
    if (f.type === 'select') {
      input = h('select', {}, (f.options || []).map((o) => h('option', { value: o.value ?? o }, o.label ?? o)));
      input.value = v == null ? '' : String(v);
    } else if (f.type === 'textarea') { input = h('textarea', { placeholder: f.placeholder || '' }); input.value = v || ''; }
    else if (f.type === 'checkbox') { input = h('input', { type: 'checkbox', style: 'width:auto' }); input.checked = !!v; }
    else { input = h('input', { type: f.type || 'text', placeholder: f.placeholder || '', autocomplete: f.type === 'password' ? 'new-password' : 'off' }); input.value = v == null ? '' : v; }
    inputs[f.name] = input;
    const wrap = h('div', {}, h('label', {}, f.label || f.name), input);
    if (f.half) { if (!row) { row = h('div', { class: 'row2' }); el.append(row); } row.append(wrap); if (!fields[i + 1] || !fields[i + 1].half) row = null; }
    else { row = null; el.append(wrap); }
  });
  const get = () => { const o = {}; for (const f of fields) { const i = inputs[f.name]; let v = f.type === 'checkbox' ? i.checked : i.value; if (f.type === 'number') v = v === '' ? null : Number(v); if (f.skipEmpty && v === '') continue; if (f.nullEmpty && v === '') v = null; o[f.name] = v; } return o; };
  return { el, get, inputs };
}
function table(cols, rows, empty) {
  if (!rows.length) return h('div', { class: 'card empty' }, empty || 'Nothing here yet.');
  return h('div', { class: 'card tablewrap' }, h('table', {}, h('thead', {}, h('tr', {}, cols.map((c) => h('th', {}, c.title)))), h('tbody', {}, rows.map((r) => h('tr', {}, cols.map((c) => h('td', {}, c.render(r))))))));
}
const all = async (res) => (await api(`/${res}/?page_size=200`)).results;
const nameOf = (list, id) => (list.find((x) => x.id === id) || {}).name || '—';

// ---------- shell ----------
const NAV = [['dashboard', 'Dashboard'], ['jobs', 'Jobs'], ['templates', 'Templates'], ['workflows', 'Workflows'], ['schedules', 'Schedules'], ['projects', 'Projects'], ['inventories', 'Inventories'], ['credentials', 'Credentials'], ['notifications', 'Notifications'], ['activity', 'Activity'], ['teams', 'Teams'], ['workers', 'Workers'], ['users', 'Users']];
function shell(route, content) {
  closeLive();
  $app.replaceChildren(h('div', { class: 'layout' },
    h('nav', { class: 'side' }, h('div', { class: 'brand' }, h('i', {}, 'R'), 'Ransible'),
      NAV.filter(([k]) => !['users', 'teams', 'notifications', 'workers'].includes(k) || me.role === 'admin').map(([k, t]) => h('a', { class: 'nav' + (route === k ? ' active' : ''), href: '#/' + k }, t)),
      h('div', { class: 'spacer' }), h('div', { class: 'who' }, me.username + ' · ' + me.role),
      h('a', { class: 'nav', href: '#/account' }, 'Account'), h('a', { class: 'nav', href: '#', onclick: (e) => { e.preventDefault(); logout(); } }, 'Sign out')),
    h('main', { class: 'main' }, content)));
}
function closeLive() { if (stream) { stream.close(); stream = null; } if (poll) { clearInterval(poll); poll = null; } }
function logout(silent) { if (token && !silent) api('/auth/logout/', { method: 'POST' }).catch(() => {}); token = ''; me = null; localStorage.removeItem('rs_token'); closeLive(); loginView(); }
function loginView() {
  const f = form([{ name: 'username', label: 'Username' }, { name: 'password', label: 'Password', type: 'password' }]);
  const go = guard(async () => { const r = await api('/auth/login/', { method: 'POST', body: f.get() }); token = r.token; localStorage.setItem('rs_token', token); me = r.user; route(); });
  f.inputs.password.addEventListener('keydown', (e) => e.key === 'Enter' && go());
  $app.replaceChildren(h('div', { class: 'login' }, h('div', { class: 'card' }, h('h1', {}, h('span', { class: 'brand' }, h('i', {}, 'R')), 'Ransible'), f.el, h('div', { class: 'foot', style: 'margin-top:16px' }, h('button', { class: 'primary', style: 'width:100%', onclick: go }, 'Sign in')))));
}

// ---------- views ----------
async function dashboard() {
  const d = await api('/dashboard/');
  const colors = { successful: 'var(--ok)', failed: 'var(--bad)', error: 'var(--bad)', running: 'var(--info)', pending: 'var(--warn)', canceled: 'var(--muted)' };
  const total = d.jobs_by_status.reduce((a, b) => a + b.n, 0) || 1;
  shell('dashboard', [h('h1', {}, 'Dashboard'),
    h('div', { class: 'cards' }, Object.entries(d.counts).map(([k, v]) => h('div', { class: 'card stat' }, h('b', {}, v), h('span', {}, k.replace('_', ' '))))),
    h('h2', {}, 'Job status'), h('div', { class: 'card', style: 'padding:14px 16px' },
      h('div', { class: 'bars' }, d.jobs_by_status.map((s) => h('span', { style: `width:${(100 * s.n) / total}%;background:${colors[s.status] || 'var(--muted)'}`, title: s.status + ': ' + s.n }))),
      h('div', { class: 'legend' }, d.jobs_by_status.map((s) => h('span', {}, h('i', { style: 'background:' + (colors[s.status] || 'var(--muted)') }), s.status + ' ' + s.n)))),
    h('h2', {}, 'Recent jobs'), jobsTable(d.recent_jobs)]);
}
function jobsTable(rows) {
  return table([
    { title: 'ID', render: (j) => h('a', { href: '#/jobs/' + j.id }, '#' + j.id) },
    { title: 'Name', render: (j) => j.name }, { title: 'Status', render: (j) => badge(j.status) },
    { title: 'By', render: (j) => j.launched_by }, { title: 'Created', render: (j) => when(j.created) }, { title: 'Finished', render: (j) => when(j.finished) },
  ], rows, 'No jobs have run yet.');
}
async function jobs() {
  const rows = (await api('/jobs/?page_size=100&order_by=-id')).results;
  shell('jobs', [h('div', { class: 'bar' }, h('h1', {}, 'Jobs')), jobsTable(rows)]);
  poll = setInterval(() => { if (rows.some((j) => j.status === 'running' || j.status === 'pending')) route(); }, 3000);
}
async function jobDetail(id) {
  let job = await api('/jobs/' + id + '/');
  const out = h('pre', { class: 'out', 'aria-live': 'polite' });
  const head = h('div', {});
  const renderHead = () => head.replaceChildren(h('div', { class: 'bar' }, h('h1', {}, `#${job.id} ${job.name} `, badge(job.status)), h('div', { class: 'actions' },
    ['running', 'pending'].includes(job.status) && can('operate') && h('button', { class: 'danger', onclick: guard(async () => { await api(`/jobs/${id}/cancel/`, { method: 'POST' }); toast('Cancel requested'); }) }, 'Cancel'),
    can('operate') && h('button', { onclick: guard(async () => { const j = await api(`/jobs/${id}/relaunch/`, { method: 'POST' }); location.hash = '#/jobs/' + j.id; }) }, 'Relaunch'))),
    h('div', { class: 'card', style: 'margin-bottom:14px' }, h('dl', { class: 'kv' }, h('dt', {}, 'Playbook'), h('dd', {}, job.playbook), h('dt', {}, 'Type'), h('dd', {}, job.job_type), h('dt', {}, 'Launched by'), h('dd', {}, job.launched_by),
      h('dt', {}, 'Started'), h('dd', {}, when(job.started)), h('dt', {}, 'Finished'), h('dd', {}, when(job.finished)),
      job.summary && job.summary.length ? [h('dt', {}, 'Hosts'), h('dd', {}, job.summary.map((s) => h('div', {}, `${s.host}: ok=${s.ok} changed=${s.changed} failed=${s.failed} unreachable=${s.unreachable} skipped=${s.skipped}`)))] : null)));
  renderHead(); shell('jobs', [head, out]);
  let follow = true; out.addEventListener('scroll', () => { follow = out.scrollTop + out.clientHeight >= out.scrollHeight - 30; });
  stream = new EventSource(`/api/v2/jobs/${id}/stream/?token=${encodeURIComponent(token)}`);
  stream.onmessage = (e) => { const m = JSON.parse(e.data); out.append(ansi(m.text)); if (follow) out.scrollTop = out.scrollHeight; };
  stream.addEventListener('done', async () => { stream.close(); stream = null; job = await api('/jobs/' + id + '/'); renderHead(); });
  stream.onerror = () => { if (stream) { stream.close(); stream = null; } };
  poll = setInterval(async () => { if (['running', 'pending'].includes(job.status)) { job = await api('/jobs/' + id + '/'); renderHead(); } }, 2500);
}

function crud(opts) {
  return async () => {
    const [rows, ctx] = await Promise.all([all(opts.res), opts.load ? opts.load() : {}]);
    const edit = (row) => {
      const f = form(opts.fields(ctx, row), row ? opts.toForm ? opts.toForm(row) : row : opts.defaults);
      modal((row ? 'Edit ' : 'New ') + opts.single, f.el, (close) => [h('button', { onclick: close }, 'Cancel'), h('button', { class: 'primary', onclick: guard(async () => {
        const body = f.get(); if (opts.beforeSave) opts.beforeSave(body, row);
        await api(row ? `/${opts.res}/${row.id}/` : `/${opts.res}/`, { method: row ? 'PUT' : 'POST', body }); close(); toast('Saved'); route(); }) }, 'Save')]);
    };
    shell(opts.res, [h('div', { class: 'bar' }, h('h1', {}, opts.title), can('write') && h('button', { class: 'primary', onclick: () => edit(null) }, 'New ' + opts.single)),
      table([...opts.columns(ctx), { title: '', render: (r) => h('div', { class: 'actions' }, (opts.extra ? opts.extra(r, ctx) : []),
        can('write') && h('button', { onclick: () => edit(r) }, 'Edit'),
        can('write') && h('button', { class: 'danger', onclick: guard(async () => { if (!confirm(`Delete ${opts.single} "${r.name || r.username}"?`)) return; await api(`/${opts.res}/${r.id}/`, { method: 'DELETE' }); toast('Deleted'); route(); }) }, 'Delete')) }], rows, opts.empty)]);
  };
}
const projects = crud({ res: 'projects', title: 'Projects', single: 'project', empty: 'Add a project: a git repository (or a local directory) containing playbooks.',
  defaults: { scm_branch: '' },
  fields: () => [{ name: 'name', label: 'Name' }, { name: 'description', label: 'Description' }, { name: 'scm_url', label: 'Git URL (leave empty for local path)', placeholder: 'https://github.com/org/repo.git' }, { name: 'scm_branch', label: 'Branch / tag', half: true }, { name: 'local_path', label: 'Local path', half: true }],
  columns: () => [{ title: 'Name', render: (p) => p.name }, { title: 'Source', render: (p) => p.scm_url || p.local_path || '—' }, { title: 'Status', render: (p) => badge(p.status === 'never updated' ? 'pending' : p.status) }, { title: 'Last sync', render: (p) => when(p.last_sync) }],
  extra: (p) => can('operate') && h('button', { onclick: guard(async (e) => { e.target.disabled = true; const r = await api(`/projects/${p.id}/sync/`, { method: 'POST' }); toast('Sync ' + r.status, r.status !== 'successful'); route(); }) }, 'Sync') });
const inventories = crud({ res: 'inventories', title: 'Inventories', single: 'inventory', empty: 'Add an inventory (INI or YAML), or an executable script for dynamic inventory.',
  defaults: { kind: 'static', content: '[web]\nhost1 ansible_host=10.0.0.1\n' },
  fields: () => [{ name: 'name', label: 'Name' }, { name: 'description', label: 'Description' }, { name: 'kind', label: 'Kind', type: 'select', options: ['static', 'script'] }, { name: 'content', label: 'Content (INI/YAML, or script source)', type: 'textarea' }],
  columns: () => [{ title: 'Name', render: (i) => i.name }, { title: 'Kind', render: (i) => i.kind }, { title: 'Description', render: (i) => i.description }],
  extra: (i) => i.kind === 'static' && h('button', { onclick: guard(async () => { const r = await api(`/inventories/${i.id}/hosts/`); modal('Hosts in ' + i.name, h('div', {}, h('p', { class: 'muted' }, `${r.hosts.length} hosts`), h('pre', { class: 'out' }, JSON.stringify(r.groups, null, 2))), (c) => [h('button', { onclick: c }, 'Close')]); }) }, 'Hosts') });
const credentials = crud({ res: 'credentials', title: 'Credentials', single: 'credential', empty: 'Add SSH/machine credentials or a vault password. Secrets are stored encrypted and never shown again.',
  defaults: { kind: 'machine' },
  toForm: (c) => ({ ...c, ssh_private_key: '', password: '', become_password: '', vault_password: '' }),
  fields: () => [{ name: 'name', label: 'Name' }, { name: 'description', label: 'Description' }, { name: 'kind', label: 'Type', type: 'select', options: ['machine', 'vault'], half: true }, { name: 'username', label: 'Username', half: true },
    { name: 'ssh_private_key', label: 'SSH private key (blank = keep)', type: 'textarea', placeholder: '-----BEGIN OPENSSH PRIVATE KEY-----' }, { name: 'password', label: 'SSH password (blank = keep)', type: 'password', half: true }, { name: 'become_password', label: 'Become password (blank = keep)', type: 'password', half: true }, { name: 'vault_password', label: 'Vault password (blank = keep)', type: 'password' }],
  beforeSave: (b) => { const s = {}; for (const k of ['ssh_private_key', 'password', 'become_password', 'vault_password']) { if (b[k]) s[k] = b[k]; delete b[k]; } if (Object.keys(s).length) b.secrets = s; },
  columns: () => [{ title: 'Name', render: (c) => c.name }, { title: 'Type', render: (c) => c.kind }, { title: 'User', render: (c) => c.username || '—' }, { title: 'Secrets', render: (c) => Object.keys(c.secrets || {}).join(', ') || '—' }] });
async function workers() {
  const rows = await all('workers');
  const add = () => { const f = form([{ name: 'name', label: 'Worker name (letters, digits, - _ .)' }]);
    modal('New worker', f.el, (close) => [h('button', { onclick: close }, 'Cancel'), h('button', { class: 'primary', onclick: guard(async () => {
      const w = await api('/workers/', { method: 'POST', body: f.get() }); close();
      const cmd = `ransible worker --server ${location.origin} --token ${w.token}`;
      modal('Worker token (shown once)', h('div', {}, h('p', {}, 'Copy this token now; it cannot be shown again. Run on the execution node:'), h('pre', { class: 'out' }, cmd), h('p', { class: 'muted' }, 'Use https:// to a non-local server: the worker receives project files and credentials.')), (c2) => [h('button', { class: 'primary', onclick: () => { c2(); route(); } }, 'Done')]);
    }) }, 'Create')]); };
  shell('workers', [h('div', { class: 'bar' }, h('h1', {}, 'Workers (remote execution nodes)'), h('button', { class: 'primary', onclick: add }, 'New worker')),
    table([{ title: 'Name', render: (w) => w.name }, { title: 'Status', render: (w) => badge(w.online ? 'successful' : 'canceled') }, { title: 'Last seen', render: (w) => when(w.last_seen) },
      { title: '', render: (w) => h('button', { class: 'danger', onclick: guard(async () => { if (!confirm(`Remove worker "${w.name}"?`)) return; await api(`/workers/${w.id}/`, { method: 'DELETE' }); route(); }) }, 'Remove') }], rows, 'Workers run jobs on other machines. Create one, then start `ransible worker` there. Set a template\'s worker to run it remotely.')]);
}
const OBJ_TYPES = [['job_templates', 'Job template'], ['workflow_templates', 'Workflow'], ['projects', 'Project'], ['inventories', 'Inventory'], ['credentials', 'Credential'], ['schedules', 'Schedule']];
async function accessModal(subjectType, subj) {
  const [grants, ...lists] = await Promise.all([api('/grants/?page_size=200').then((r) => r.results.filter((g) => g.subject_type === subjectType && g.subject_id === subj.id)), ...OBJ_TYPES.map(([t]) => all(t))]);
  const byType = Object.fromEntries(OBJ_TYPES.map(([t], i) => [t, lists[i]]));
  const label = (g) => (OBJ_TYPES.find((o) => o[0] === g.object_type) || [0, g.object_type])[1] + ': ' + nameOf(byType[g.object_type] || [], g.object_id);
  const f = form([{ name: 'object_type', label: 'Object type', type: 'select', options: OBJ_TYPES.map(([v, l]) => ({ value: v, label: l })), half: true }, { name: 'role', label: 'Role', type: 'select', options: [{ value: 'execute', label: 'execute (launch/sync)' }, { value: 'admin', label: 'admin (edit/delete)' }], half: true }, { name: 'object_id', label: 'Object', type: 'select', options: [] }]);
  const fillObjects = () => { const sel = f.inputs.object_id; sel.replaceChildren(...byType[f.inputs.object_type.value].map((o) => h('option', { value: o.id }, o.name))); };
  f.inputs.object_type.addEventListener('change', fillObjects); fillObjects();
  const body = h('div', {}, table([{ title: 'Object', render: (g) => label(g) }, { title: 'Role', render: (g) => g.role }, { title: '', render: (g) => h('button', { class: 'danger', onclick: guard(async () => { await api(`/grants/${g.id}/`, { method: 'DELETE' }); location.reload(); }) }, 'Revoke') }], grants, 'No object access granted.'), h('h2', {}, 'Grant access'), f.el);
  modal('Access for ' + (subj.name || subj.username), body, (close) => [h('button', { onclick: close }, 'Close'), h('button', { class: 'primary', onclick: guard(async () => { const b = f.get(); b.object_id = Number(b.object_id); await api('/grants/', { method: 'POST', body: { ...b, subject_type: subjectType, subject_id: subj.id } }); close(); toast('Granted'); accessModal(subjectType, subj); }) }, 'Grant')]);
}
const teams = crud({ res: 'teams', title: 'Teams', single: 'team', empty: 'Teams group users so you can grant them access to specific templates, projects and more.',
  defaults: {},
  fields: () => [{ name: 'name', label: 'Name' }, { name: 'description', label: 'Description' }],
  columns: () => [{ title: 'Name', render: (t) => t.name }, { title: 'Description', render: (t) => t.description }],
  extra: (t) => [h('button', { onclick: guard(async () => {
      const [members, us] = await Promise.all([api(`/teams/${t.id}/members/`).then((r) => r.results), all('users')]);
      const sel = h('select', {}, us.filter((u) => !members.some((m) => m.id === u.id)).map((u) => h('option', { value: u.id }, u.username)));
      modal('Members of ' + t.name, h('div', {}, table([{ title: 'User', render: (m) => m.username }, { title: 'Role', render: (m) => m.role }, { title: '', render: (m) => h('button', { class: 'danger', onclick: guard(async () => { await api(`/teams/${t.id}/members/${m.id}/`, { method: 'DELETE' }); toast('Removed'); route(); }) }, 'Remove') }], members, 'No members yet.'), h('h2', {}, 'Add member'), sel),
        (close) => [h('button', { onclick: close }, 'Close'), h('button', { class: 'primary', onclick: guard(async () => { if (!sel.value) return; await api(`/teams/${t.id}/members/`, { method: 'POST', body: { user_id: Number(sel.value) } }); close(); toast('Added'); route(); }) }, 'Add')]);
    }) }, 'Members'), h('button', { onclick: guard(() => accessModal('team', t)) }, 'Access')] });
const users = crud({ res: 'users', title: 'Users', single: 'user', empty: '',
  defaults: { role: 'viewer' },
  toForm: (u) => ({ ...u, password: '' }),
  fields: (_c, row) => [{ name: 'username', label: 'Username' }, { name: 'role', label: 'Role', type: 'select', options: ['viewer', 'operator', 'admin'] }, { name: 'password', label: row ? 'New password (blank = keep)' : 'Password (min 8 chars)', type: 'password' }],
  beforeSave: (b) => { if (!b.password) delete b.password; },
  columns: () => [{ title: 'Username', render: (u) => u.username }, { title: 'Role', render: (u) => u.role }, { title: 'Created', render: (u) => when(u.created) }],
  extra: (u) => h('button', { onclick: guard(() => accessModal('user', u)) }, 'Access') });
// ----- workflows -----
const STATE_COLOR = { successful: 'var(--ok)', failed: 'var(--bad)', denied: 'var(--bad)', running: 'var(--info)', waiting: 'var(--warn)', pending: 'var(--muted)', skipped: 'var(--muted)', canceled: 'var(--muted)' };
function dag(graph, state, onNode) {
  const NS = 'http://www.w3.org/2000/svg';
  const mk = (t, a, ...k) => { const e = document.createElementNS(NS, t); for (const [x, y] of Object.entries(a || {})) e.setAttribute(x, y); k.forEach((c) => e.append(c)); return e; };
  const layer = {}; const kids = {}; (graph.edges || []).forEach((e) => (kids[e.from] = (kids[e.from] || []).concat(e)));
  const incoming = {}; (graph.edges || []).forEach((e) => (incoming[e.to] = (incoming[e.to] || 0) + 1));
  const visit = (k, d) => { if ((layer[k] ?? -1) >= d) return; layer[k] = d; (kids[k] || []).forEach((e) => visit(e.to, d + 1)); };
  (graph.nodes || []).filter((n) => !incoming[n.key]).forEach((n) => visit(n.key, 0));
  const cols = {}; (graph.nodes || []).forEach((n) => { const l = layer[n.key] ?? 0; (cols[l] = cols[l] || []).push(n); });
  const W = 150, H = 44, GX = 70, GY = 26, pos = {};
  Object.entries(cols).forEach(([l, ns]) => ns.forEach((n, i) => (pos[n.key] = { x: 16 + l * (W + GX), y: 16 + i * (H + GY) })));
  const width = 32 + (Math.max(0, ...Object.keys(cols).map(Number)) + 1) * (W + GX), height = 32 + Math.max(1, ...Object.values(cols).map((c) => c.length)) * (H + GY);
  const col = { success: 'var(--ok)', failure: 'var(--bad)', always: 'var(--muted)' };
  const svg = mk('svg', { width, height, viewBox: `0 0 ${width} ${height}`, role: 'img', 'aria-label': 'Workflow graph' });
  (graph.edges || []).forEach((e) => { const a = pos[e.from], b = pos[e.to]; if (!a || !b) return;
    const x1 = a.x + W, y1 = a.y + H / 2, x2 = b.x, y2 = b.y + H / 2, mx = (x1 + x2) / 2;
    svg.append(mk('path', { d: `M${x1},${y1} C${mx},${y1} ${mx},${y2} ${x2 - 6},${y2}`, fill: 'none', stroke: col[e.kind], 'stroke-width': 2, 'stroke-dasharray': e.kind === 'always' ? '5 4' : '' }));
    svg.append(mk('path', { d: `M${x2},${y2} l-8,-4 l0,8 z`, fill: col[e.kind] })); });
  (graph.nodes || []).forEach((n) => { const p = pos[n.key]; const st = state && state[n.key] ? state[n.key].status : null;
    const g = mk('g', { style: onNode ? 'cursor:pointer' : '', tabindex: onNode ? 0 : -1 });
    g.append(mk('rect', { x: p.x, y: p.y, width: W, height: H, rx: 9, fill: 'var(--panel)', stroke: st ? STATE_COLOR[st] : 'var(--border)', 'stroke-width': st ? 2.5 : 1.5 }));
    const t = mk('text', { x: p.x + 10, y: p.y + 19, fill: 'var(--text)', 'font-size': 13, 'font-weight': 600 }); t.textContent = (n.label || n.key).slice(0, 20); g.append(t);
    const t2 = mk('text', { x: p.x + 10, y: p.y + 35, fill: st ? STATE_COLOR[st] : 'var(--muted)', 'font-size': 11 }); t2.textContent = (n.kind === 'approval' ? 'approval' : 'job') + (st ? ' · ' + st : ''); g.append(t2);
    if (onNode) g.addEventListener('click', () => onNode(n, st)); svg.append(g); });
  return h('div', { class: 'card', style: 'overflow:auto;padding:8px' }, svg);
}
async function workflows() {
  const [rows, tpls, wjobs] = await Promise.all([all('workflow_templates'), all('job_templates'), api('/workflow_jobs/?page_size=10&order_by=-id').then((r) => r.results)]);
  const edit = async (row) => {
    const f = form([{ name: 'name', label: 'Name' }, { name: 'description', label: 'Description' }], row);
    const g = row ? await api(`/workflow_templates/${row.id}/graph/`) : { nodes: [], edges: [] };
    const nodes = g.nodes.map((n) => ({ ...n })), edges = g.edges.map((e) => ({ ...e }));
    const area = h('div', {});
    const sel = (opts, val, on) => { const s = h('select', {}, opts.map((o) => h('option', { value: o.value }, o.label))); s.value = val ?? ''; s.addEventListener('change', () => on(s.value)); return s; };
    const draw = () => {
      area.replaceChildren(h('h2', {}, 'Nodes'),
        ...nodes.map((n, i) => h('div', { class: 'row2', style: 'align-items:end;margin-bottom:6px' }, h('div', {}, h('label', {}, 'Key / label'), h('input', { value: n.key, oninput: (e) => { const old = n.key; n.key = e.target.value; edges.forEach((x) => { if (x.from === old) x.from = n.key; if (x.to === old) x.to = n.key; }); if (!n.label || n.label === old) n.label = n.key; } })),
          h('div', { class: 'actions' }, sel([{ value: 'job', label: 'job' }, { value: 'approval', label: 'approval' }], n.kind, (v) => { n.kind = v; draw(); }),
            n.kind === 'job' && sel([{ value: '', label: '— template —' }, ...tpls.map((t) => ({ value: t.id, label: t.name }))], n.template_id, (v) => (n.template_id = v ? Number(v) : null)),
            h('button', { class: 'danger', onclick: () => { const k = n.key; nodes.splice(i, 1); for (let j = edges.length - 1; j >= 0; j--) if (edges[j].from === k || edges[j].to === k) edges.splice(j, 1); draw(); } }, '✕')))),
        h('button', { onclick: () => { let k = 'n' + (nodes.length + 1); while (nodes.some((x) => x.key === k)) k += '_'; nodes.push({ key: k, label: k, kind: 'job', template_id: null }); draw(); } }, '+ Node'),
        h('h2', {}, 'Edges (run "to" when "from" ...)'),
        ...edges.map((e, i) => h('div', { class: 'actions', style: 'margin-bottom:6px' }, sel(nodes.map((n) => ({ value: n.key, label: n.key })), e.from, (v) => (e.from = v)), sel([{ value: 'success', label: 'succeeds →' }, { value: 'failure', label: 'fails →' }, { value: 'always', label: 'finishes →' }], e.kind, (v) => (e.kind = v)), sel(nodes.map((n) => ({ value: n.key, label: n.key })), e.to, (v) => (e.to = v)), h('button', { class: 'danger', onclick: () => { edges.splice(i, 1); draw(); } }, '✕'))),
        h('button', { onclick: () => { if (nodes.length > 1) { edges.push({ from: nodes[0].key, to: nodes[1].key, kind: 'success' }); draw(); } } }, '+ Edge'),
        h('h2', {}, 'Preview'), dag({ nodes, edges }, null));
    };
    draw(); const body = h('div', {}, f.el, area);
    modal(row ? 'Edit workflow' : 'New workflow', body, (close) => [h('button', { onclick: close }, 'Cancel'), h('button', { class: 'primary', onclick: guard(async () => {
      const w = row ? await api(`/workflow_templates/${row.id}/`, { method: 'PUT', body: f.get() }) : await api('/workflow_templates/', { method: 'POST', body: f.get() });
      await api(`/workflow_templates/${w.id}/graph/`, { method: 'PUT', body: { nodes, edges } }); close(); toast('Saved'); route(); }) }, 'Save')]);
  };
  shell('workflows', [h('div', { class: 'bar' }, h('h1', {}, 'Workflow templates'), can('write') && h('button', { class: 'primary', onclick: () => edit(null) }, 'New workflow')),
    table([{ title: 'Name', render: (w) => w.name }, { title: 'Description', render: (w) => w.description },
      { title: '', render: (w) => h('div', { class: 'actions' }, can('operate') && h('button', { class: 'primary', onclick: guard(async () => { const r = await api(`/workflow_templates/${w.id}/launch/`, { method: 'POST' }); location.hash = '#/wfjob/' + r.id; }) }, 'Launch'), can('write') && h('button', { onclick: guard(() => edit(w)) }, 'Edit'),
        can('write') && h('button', { class: 'danger', onclick: guard(async () => { if (!confirm(`Delete workflow "${w.name}"?`)) return; await api(`/workflow_templates/${w.id}/`, { method: 'DELETE' }); route(); }) }, 'Delete')) }], rows, 'Chain job templates with success/failure/always paths and approval gates.'),
    h('h2', {}, 'Recent workflow runs'), table([{ title: 'ID', render: (j) => h('a', { href: '#/wfjob/' + j.id }, '#' + j.id) }, { title: 'Workflow', render: (j) => j.name }, { title: 'Status', render: (j) => badge(j.status) }, { title: 'By', render: (j) => j.launched_by }, { title: 'Created', render: (j) => when(j.created) }], wjobs, 'No workflow runs yet.')]);
}
async function wfJob(id) {
  let wj = await api('/workflow_jobs/' + id + '/');
  const holder = h('div', {});
  const render = () => holder.replaceChildren(h('div', { class: 'bar' }, h('h1', {}, `Workflow #${wj.id} ${wj.name} `, badge(wj.status)), h('div', { class: 'actions' }, ['running', 'pending'].includes(wj.status) && can('operate') && h('button', { class: 'danger', onclick: guard(async () => { await api(`/workflow_jobs/${id}/cancel/`, { method: 'POST' }); toast('Cancel requested'); }) }, 'Cancel'))),
    dag(wj.graph, wj.state, (n, st) => { if (st === 'waiting' && can('operate')) { modal('Approval: ' + (n.label || n.key), h('p', {}, 'This workflow is waiting for a decision.'), (close) => [h('button', { class: 'danger', onclick: guard(async () => { await api(`/workflow_jobs/${id}/nodes/${n.key}/deny/`, { method: 'POST' }); close(); }) }, 'Deny'), h('button', { class: 'primary', onclick: guard(async () => { await api(`/workflow_jobs/${id}/nodes/${n.key}/approve/`, { method: 'POST' }); close(); }) }, 'Approve')]); } else if (wj.state[n.key] && wj.state[n.key].job_id) location.hash = '#/jobs/' + wj.state[n.key].job_id; }),
    h('p', { class: 'muted' }, 'Click a node to open its job output; click a waiting approval node to approve or deny.'),
    h('dl', { class: 'kv card' }, h('dt', {}, 'Launched by'), h('dd', {}, wj.launched_by), h('dt', {}, 'Started'), h('dd', {}, when(wj.started)), h('dt', {}, 'Finished'), h('dd', {}, when(wj.finished))));
  render(); shell('workflows', holder);
  poll = setInterval(async () => { if (['running', 'pending'].includes(wj.status) || Object.values(wj.state || {}).some((s) => s.status === 'waiting')) { wj = await api('/workflow_jobs/' + id + '/'); render(); } }, 1500);
}
const schedules = async () => {
  const tpls = await all('job_templates');
  return crud({ res: 'schedules', title: 'Schedules', single: 'schedule', empty: 'Schedules launch a template on a cron expression (UTC), e.g. */15 * * * * or @daily.',
    defaults: { cron: '0 * * * *', enabled: true },
    toForm: (s) => ({ ...s, enabled: !!s.enabled }),
    fields: () => [{ name: 'name', label: 'Name' }, { name: 'template_id', label: 'Job template', type: 'select', options: tpls.map((t) => ({ value: t.id, label: t.name })), nullEmpty: true },
      { name: 'cron', label: 'Cron (min hour day month weekday, UTC)', placeholder: '*/15 * * * *' }, { name: 'extra_vars', label: 'Extra variables (YAML/JSON, merged over the template)', type: 'textarea' }, { name: 'enabled', label: 'Enabled', type: 'checkbox' }],
    columns: () => [{ title: 'Name', render: (s) => s.name }, { title: 'Template', render: (s) => nameOf(tpls, s.template_id) }, { title: 'Cron', render: (s) => h('code', {}, s.cron) }, { title: 'Enabled', render: (s) => (s.enabled ? 'yes' : 'no') }, { title: 'Last run', render: (s) => when(s.last_run) }] })();
};
const notifications = crud({ res: 'notification_templates', title: 'Notification templates', single: 'notification', empty: 'Send a webhook or Slack message when jobs start, succeed or fail.',
  defaults: { kind: 'webhook', enabled: true, on_failure: true },
  toForm: (n) => ({ ...n, webhook_url: '', enabled: !!n.enabled, on_start: !!n.on_start, on_success: !!n.on_success, on_failure: !!n.on_failure }),
  fields: (_c, row) => [{ name: 'name', label: 'Name' }, { name: 'kind', label: 'Type', type: 'select', options: ['webhook', 'slack'], half: true }, { name: 'webhook_url', label: row ? 'URL (blank = keep)' : 'URL', half: true, skipEmpty: !!row },
    { name: 'on_start', label: 'On start', type: 'checkbox', half: true }, { name: 'on_success', label: 'On success', type: 'checkbox', half: true }, { name: 'on_failure', label: 'On failure', type: 'checkbox', half: true }, { name: 'enabled', label: 'Enabled', type: 'checkbox', half: true }],
  columns: () => [{ title: 'Name', render: (n) => n.name }, { title: 'Type', render: (n) => n.kind }, { title: 'When', render: (n) => [n.on_start && 'start', n.on_success && 'success', n.on_failure && 'failure'].filter(Boolean).join(', ') }, { title: 'Enabled', render: (n) => (n.enabled ? 'yes' : 'no') }] });
async function activity() {
  const rows = (await api('/activity_stream/?page_size=100&order_by=-id')).results;
  shell('activity', [h('h1', {}, 'Activity stream'), table([{ title: 'Time', render: (a) => when(a.ts) }, { title: 'User', render: (a) => a.username }, { title: 'Action', render: (a) => a.action }, { title: 'Object', render: (a) => a.object_type + (a.object_id ? ' #' + a.object_id : '') }, { title: 'Summary', render: (a) => a.summary }], rows, 'No activity yet.')]);
}
const templates = async () => {
  const [rows, projs, invs, creds, wks] = await Promise.all([all('job_templates'), all('projects'), all('inventories'), all('credentials'), all('workers')]);
  const opt = (l, none) => [{ value: '', label: none }, ...l.map((x) => ({ value: x.id, label: x.name }))];
  const edit = async (row) => {
    const f = form([{ name: 'name', label: 'Name' }, { name: 'description', label: 'Description' },
      { name: 'project_id', label: 'Project', type: 'select', options: opt(projs, '— select —'), half: true, nullEmpty: true }, { name: 'playbook', label: 'Playbook (path in project)', half: true, placeholder: 'site.yml' },
      { name: 'inventory_id', label: 'Inventory', type: 'select', options: opt(invs, '— select —'), half: true, nullEmpty: true }, { name: 'credential_id', label: 'Credential', type: 'select', options: opt(creds.filter((c) => c.kind === 'machine'), '— none —'), half: true, nullEmpty: true },
      { name: 'vault_credential_id', label: 'Vault credential', type: 'select', options: opt(creds.filter((c) => c.kind === 'vault'), '— none —'), nullEmpty: true },
      { name: 'extra_vars', label: 'Extra variables (YAML/JSON)', type: 'textarea' }, { name: 'limit_hosts', label: 'Limit', half: true }, { name: 'tags', label: 'Tags', half: true },
      { name: 'job_type', label: 'Job type', type: 'select', options: ['run', 'check'], half: true }, { name: 'forks', label: 'Forks', type: 'number', half: true },
      { name: 'worker', label: 'Run on', type: 'select', options: [{ value: '', label: 'this server' }, ...wks.map((w) => ({ value: w.name, label: 'worker: ' + w.name + (w.online ? '' : ' (offline)') }))] }, { name: 'become_enabled', label: 'Privilege escalation (become)', type: 'checkbox', half: true }, { name: 'diff_mode', label: 'Show diffs', type: 'checkbox', half: true },
      { name: 'survey_enabled', label: 'Enable survey (prompt at launch)', type: 'checkbox' }, { name: 'survey_spec', label: 'Survey questions (JSON list: variable, question, type, required, default, choices, min, max)', type: 'textarea', placeholder: '[{"variable":"env","question":"Environment","type":"multiplechoice","choices":["dev","prod"],"default":"dev"}]' }],
      row ? { ...row, become_enabled: !!row.become_enabled, diff_mode: !!row.diff_mode, survey_enabled: !!row.survey_enabled } : { job_type: 'run', forks: 5 });
    const hint = h('div', { class: 'muted', style: 'font-size:12px;margin-top:4px' });
    const pbSel = f.inputs.playbook;
    f.inputs.project_id.addEventListener('change', guard(async () => { const id = f.inputs.project_id.value; if (!id) return; const pbs = await api(`/projects/${id}/playbooks/`); hint.textContent = pbs.length ? 'Playbooks found: ' + pbs.join(', ') : 'No playbooks found — sync the project first.'; if (!pbSel.value && pbs[0]) pbSel.value = pbs[0]; }));
    f.el.append(hint);
    modal(row ? 'Edit template' : 'New template', f.el, (close) => [h('button', { onclick: close }, 'Cancel'), h('button', { class: 'primary', onclick: guard(async () => { const body = f.get(); await api(row ? `/job_templates/${row.id}/` : '/job_templates/', { method: row ? 'PUT' : 'POST', body }); close(); toast('Saved'); route(); }) }, 'Save')]);
  };
  const launch = (t) => {
    let spec = []; if (t.survey_enabled) { try { spec = JSON.parse(t.survey_spec || '[]'); } catch (e) { spec = []; } }
    const sfields = spec.map((q) => {
      const ch = Array.isArray(q.choices) ? q.choices : String(q.choices || '').split('\n').filter(Boolean);
      const base = { name: q.variable, label: (q.question || q.variable) + (q.required ? ' *' : ''), default: q.default };
      if (q.type === 'multiplechoice') return { ...base, type: 'select', options: ch };
      if (q.type === 'password') return { ...base, type: 'password' };
      if (q.type === 'integer' || q.type === 'float') return { ...base, type: 'number' };
      return base; // text, multiselect (comma separated)
    });
    const sf = sfields.length ? form(sfields) : null;
    const f = form([{ name: 'extra_vars', label: 'Extra variables (YAML/JSON)', type: 'textarea', default: t.extra_vars }, { name: 'limit', label: 'Limit', default: t.limit_hosts, half: true }, { name: 'job_type', label: 'Job type', type: 'select', options: ['run', 'check'], default: t.job_type, half: true }]);
    modal('Launch ' + t.name, h('div', {}, sf && h('div', {}, h('h2', {}, 'Survey'), sf.el), f.el), (close) => [h('button', { onclick: close }, 'Cancel'), h('button', { class: 'primary', onclick: guard(async () => {
      const body = f.get(); if (sf) { let ev = {}; try { ev = JSON.parse(body.extra_vars || '{}'); } catch (e) { ev = {}; } body.extra_vars = { ...ev, ...sf.get() }; }
      const j = await api(`/job_templates/${t.id}/launch/`, { method: 'POST', body }); close(); location.hash = '#/jobs/' + j.id; }) }, 'Launch')]);
  };
  shell('templates', [h('div', { class: 'bar' }, h('h1', {}, 'Job templates'), can('write') && h('button', { class: 'primary', onclick: () => edit(null) }, 'New template')),
    table([{ title: 'Name', render: (t) => t.name }, { title: 'Project', render: (t) => nameOf(projs, t.project_id) }, { title: 'Playbook', render: (t) => t.playbook }, { title: 'Inventory', render: (t) => nameOf(invs, t.inventory_id) },
      { title: '', render: (t) => h('div', { class: 'actions' }, can('operate') && h('button', { class: 'primary', onclick: () => launch(t) }, 'Launch'), can('write') && h('button', { onclick: () => edit(t) }, 'Edit'),
        can('write') && h('button', { class: 'danger', onclick: guard(async () => { if (!confirm(`Delete template "${t.name}"?`)) return; await api(`/job_templates/${t.id}/`, { method: 'DELETE' }); route(); }) }, 'Delete')) }], rows, 'Create a template from a project, playbook and inventory, then launch it.')]);
};
function account() {
  const f = form([{ name: 'old_password', label: 'Current password', type: 'password' }, { name: 'new_password', label: 'New password (min 8 chars)', type: 'password' }]);
  shell('account', [h('h1', {}, 'Account'), h('div', { class: 'card', style: 'padding:16px;max-width:420px' }, f.el, h('div', { class: 'foot', style: 'margin-top:14px' }, h('button', { class: 'primary', onclick: guard(async () => { await api('/me/password/', { method: 'POST', body: f.get() }); toast('Password changed'); f.inputs.old_password.value = f.inputs.new_password.value = ''; }) }, 'Change password')))]);
}

// ---------- router ----------
async function route() {
  if (!token) return loginView();
  try {
    if (!me) me = await api('/me/');
    const [r, arg] = (location.hash.replace(/^#\//, '') || 'dashboard').split('/');
    const views = { dashboard, jobs: arg ? () => jobDetail(arg) : jobs, templates, workflows, wfjob: () => wfJob(arg), schedules, projects, inventories, credentials, notifications, activity, teams, workers, users, account };
    await (views[r] || dashboard)();
  } catch (e) { if (token) toast(e.message, true); }
}
window.addEventListener('hashchange', route);
route();
})();
