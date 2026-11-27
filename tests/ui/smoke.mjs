// UI smoke test: loads the embedded SPA in jsdom against a running `ransible serve`.
// Usage: BASE=http://127.0.0.1:8080 ADMIN_PW=... node smoke.mjs /path/to/project-with-site.yml
import { JSDOM } from 'jsdom';
import fs from 'node:fs';
const base = process.env.BASE || 'http://127.0.0.1:18099';
const adminPw = process.env.ADMIN_PW || 'uipass-12345';
const errors = [];
const dom = new JSDOM(await (await fetch(base + '/')).text(), { url: base + '/', runScripts: 'outside-only', pretendToBeVisual: true });
const w = dom.window;
w.fetch = (u, o) => fetch(new URL(u, base), o);
w.EventSource = class { constructor() { setTimeout(() => this.onerror && this.onerror(), 10); } close() {} addEventListener() {} };
w.confirm = () => true;
w.addEventListener('error', (e) => errors.push('window error: ' + e.message));
w.eval(fs.readFileSync(new URL('../../web/app.js', import.meta.url), 'utf8'));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const $ = (s) => w.document.querySelector(s);
const text = () => w.document.body.textContent;
await sleep(300);
if (!text().includes('Sign in')) errors.push('no login view');
$('input[type=text]').value = 'admin'; $('input[type=password]').value = adminPw;
[...w.document.querySelectorAll('button')].find((b) => b.textContent === 'Sign in').click();
await sleep(4000);
if (!text().includes('Dashboard')) errors.push('login failed: ' + (w.document.querySelector('.toast')?.textContent) + ' / ' + $('input[type=text]').value + '/' + $('input[type=password]').value);
// create via API then visit every view (argv[2] = project directory containing site.yml)
const tok = w.localStorage.getItem('rs_token');
const api = (p, m = 'GET', b) => fetch(base + '/api/v2' + p, { method: m, headers: { 'Content-Type': 'application/json', Authorization: 'Bearer ' + tok }, body: b && JSON.stringify(b) }).then((r) => r.json());
const proj = await api('/projects/', 'POST', { name: 'p', local_path: process.argv[2] });
const inv = await api('/inventories/', 'POST', { name: 'i', content: 'localhost ansible_connection=local\n' });
await api('/credentials/', 'POST', { name: 'c', username: 'u', secrets: { password: 'x' } });
const tpl = await api('/job_templates/', 'POST', { name: 't', project_id: proj.id, playbook: 'site.yml', inventory_id: inv.id });
const job = await api(`/job_templates/${tpl.id}/launch/`, 'POST', {});
const wf = await api('/workflow_templates/', 'POST', { name: 'w' });
await api(`/workflow_templates/${wf.id}/graph/`, 'PUT', { nodes: [{ key: 'a', label: 'a', template_id: tpl.id }, { key: 'gate', kind: 'approval' }, { key: 'b', label: 'b', template_id: tpl.id }], edges: [{ from: 'a', to: 'gate', kind: 'success' }, { from: 'gate', to: 'b', kind: 'success' }] });
const wj = await api(`/workflow_templates/${wf.id}/launch/`, 'POST', {});
await sleep(2500);
for (const v of ['dashboard', 'workflows', 'wfjob/' + wj.id, 'jobs', 'jobs/' + job.id, 'templates', 'schedules', 'projects', 'inventories', 'credentials', 'notifications', 'activity', 'users', 'account']) {
  w.location.hash = '#/' + v; await sleep(500);
  const t = text();
  if (w.document.querySelector('.toast.err')) errors.push(v + ': error toast: ' + w.document.querySelector('.toast.err').textContent);
  console.log(v.padEnd(12), t.replace(/\s+/g, ' ').slice(0, 110));
}
const pre = w.document.querySelector('pre.out');
w.location.hash = '#/jobs/' + job.id; await sleep(600);
console.log('job badge:', w.document.querySelector('.badge')?.textContent);
// open + submit the "New project" modal
w.location.hash = '#/projects'; await sleep(400);
[...w.document.querySelectorAll('button')].find((b) => b.textContent === 'New project').click();
console.log('modal:', !!$('.modal'));
console.log(errors.length ? 'ERRORS:\n' + errors.join('\n') : 'UI smoke OK');
process.exit(errors.length ? 1 : 0);
