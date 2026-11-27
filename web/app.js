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
const NAV = [['dashboard', 'Dashboard'], ['jobs', 'Jobs'], ['templates', 'Templates'], ['projects', 'Projects'], ['inventories', 'Inventories'], ['credentials', 'Credentials'], ['users', 'Users']];
function shell(route, content) {
  closeLive();
  $app.replaceChildren(h('div', { class: 'layout' },
    h('nav', { class: 'side' }, h('div', { class: 'brand' }, h('i', {}, 'R'), 'Ransible'),
      NAV.filter(([k]) => k !== 'users' || me.role === 'admin').map(([k, t]) => h('a', { class: 'nav' + (route === k ? ' active' : ''), href: '#/' + k }, t)),
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
const users = crud({ res: 'users', title: 'Users', single: 'user', empty: '',
  defaults: { role: 'viewer' },
  toForm: (u) => ({ ...u, password: '' }),
  fields: (_c, row) => [{ name: 'username', label: 'Username' }, { name: 'role', label: 'Role', type: 'select', options: ['viewer', 'operator', 'admin'] }, { name: 'password', label: row ? 'New password (blank = keep)' : 'Password (min 8 chars)', type: 'password' }],
  beforeSave: (b) => { if (!b.password) delete b.password; },
  columns: () => [{ title: 'Username', render: (u) => u.username }, { title: 'Role', render: (u) => u.role }, { title: 'Created', render: (u) => when(u.created) }] });
const templates = async () => {
  const [rows, projs, invs, creds] = await Promise.all([all('job_templates'), all('projects'), all('inventories'), all('credentials')]);
  const opt = (l, none) => [{ value: '', label: none }, ...l.map((x) => ({ value: x.id, label: x.name }))];
  const edit = async (row) => {
    const f = form([{ name: 'name', label: 'Name' }, { name: 'description', label: 'Description' },
      { name: 'project_id', label: 'Project', type: 'select', options: opt(projs, '— select —'), half: true, nullEmpty: true }, { name: 'playbook', label: 'Playbook (path in project)', half: true, placeholder: 'site.yml' },
      { name: 'inventory_id', label: 'Inventory', type: 'select', options: opt(invs, '— select —'), half: true, nullEmpty: true }, { name: 'credential_id', label: 'Credential', type: 'select', options: opt(creds.filter((c) => c.kind === 'machine'), '— none —'), half: true, nullEmpty: true },
      { name: 'vault_credential_id', label: 'Vault credential', type: 'select', options: opt(creds.filter((c) => c.kind === 'vault'), '— none —'), nullEmpty: true },
      { name: 'extra_vars', label: 'Extra variables (YAML/JSON)', type: 'textarea' }, { name: 'limit_hosts', label: 'Limit', half: true }, { name: 'tags', label: 'Tags', half: true },
      { name: 'job_type', label: 'Job type', type: 'select', options: ['run', 'check'], half: true }, { name: 'forks', label: 'Forks', type: 'number', half: true }, { name: 'become_enabled', label: 'Privilege escalation (become)', type: 'checkbox', half: true }, { name: 'diff_mode', label: 'Show diffs', type: 'checkbox', half: true }],
      row ? { ...row, become_enabled: !!row.become_enabled, diff_mode: !!row.diff_mode } : { job_type: 'run', forks: 5 });
    const hint = h('div', { class: 'muted', style: 'font-size:12px;margin-top:4px' });
    const pbSel = f.inputs.playbook;
    f.inputs.project_id.addEventListener('change', guard(async () => { const id = f.inputs.project_id.value; if (!id) return; const pbs = await api(`/projects/${id}/playbooks/`); hint.textContent = pbs.length ? 'Playbooks found: ' + pbs.join(', ') : 'No playbooks found — sync the project first.'; if (!pbSel.value && pbs[0]) pbSel.value = pbs[0]; }));
    f.el.append(hint);
    modal(row ? 'Edit template' : 'New template', f.el, (close) => [h('button', { onclick: close }, 'Cancel'), h('button', { class: 'primary', onclick: guard(async () => { const body = f.get(); await api(row ? `/job_templates/${row.id}/` : '/job_templates/', { method: row ? 'PUT' : 'POST', body }); close(); toast('Saved'); route(); }) }, 'Save')]);
  };
  const launch = (t) => {
    const f = form([{ name: 'extra_vars', label: 'Extra variables (YAML/JSON)', type: 'textarea', default: t.extra_vars }, { name: 'limit', label: 'Limit', default: t.limit_hosts, half: true }, { name: 'job_type', label: 'Job type', type: 'select', options: ['run', 'check'], default: t.job_type, half: true }]);
    modal('Launch ' + t.name, f.el, (close) => [h('button', { onclick: close }, 'Cancel'), h('button', { class: 'primary', onclick: guard(async () => { const j = await api(`/job_templates/${t.id}/launch/`, { method: 'POST', body: f.get() }); close(); location.hash = '#/jobs/' + j.id; }) }, 'Launch')]);
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
    const views = { dashboard, jobs: arg ? () => jobDetail(arg) : jobs, templates, projects, inventories, credentials, users, account };
    await (views[r] || dashboard)();
  } catch (e) { if (token) toast(e.message, true); }
}
window.addEventListener('hashchange', route);
route();
})();
