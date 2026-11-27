# ransible

Ansible-compatible automation runner in Rust. Single binary, runs existing Ansible
inventories, playbooks and roles unchanged. No Python required on controller or targets.

```
cargo build --release
ransible playbook site.yml -i inventory.ini [-l web] [-t tag] [--skip-tags x] [-e k=v|@file] [-C] [-D] [-v] [-f 5]
ransible playbook site.yml --list-tasks | --list-hosts | --syntax-check
ransible adhoc web -i inventory -m shell -a 'uptime'
ransible inventory 'web:&prod' -i inventory
```

## Supported
- **Inventory**: INI + YAML, groups/children/vars, `host_vars/` `group_vars/`, ranges `web[01:05]`, patterns (`a:b`, `&`, `!`, globs, `~regex`), `ansible_host/port/user/connection/ssh_private_key_file/become*`.
- **Playbooks**: plays, `vars`, `vars_files`, pre/post tasks, roles (+`meta` deps, defaults, vars, templates, files), handlers (`notify`/`listen`), blocks (`rescue`/`always`), `include_tasks`/`import_tasks`/`include_role`/`import_role`/`import_playbook`, `when`, `loop`/`with_items`/`with_list`/`with_dict`, `loop_control`, `register`, `tags`, `become`, `ignore_errors`, `changed_when`/`failed_when`, `until`/`retries`/`delay`, `delegate_to`, `run_once`, `serial`, `environment`, `check_mode`, `no_log`, `meta`.
- **Templating**: Jinja2 (minijinja) + Ansible filters/tests/lookups (`default`, `to_json`, `to_nice_yaml`, `regex_replace`, `combine`, `dict2items`, `b64encode`, `mandatory`, `ternary`, `version`, `lookup('env'|'file'|'pipe')` ...), native types for `{{ expr }}`.
- **Modules** (short or `ansible.builtin.` names): command shell raw script copy template file lineinfile blockinfile replace stat slurp debug set_fact include_vars fail assert ping pause wait_for wait_for_connection setup apt yum dnf package apk homebrew service systemd user group git get_url uri fetch synchronize meta.
- **Connections**: `local`, `ssh` (system `ssh`, ControlMaster; key/agent auth, `sudo` become without password).

## Not yet supported
Galaxy collections / non-builtin modules (clear "unsupported module" error), Vault, dynamic inventory scripts/plugins, `free` strategy, become passwords, `async`/`poll`, `copy` of directories, `with_fileglob/subelements/nested`, callbacks, `ansible_facts` beyond the basic set from `setup`.

## Web UI and API (AWX-style), same binary
```
ransible serve --listen 127.0.0.1:8080 --data-dir ~/.ransible [--admin-password-file F] [--max-concurrent-jobs 4]
```
Open the printed URL. On first start an `admin` user is created (generated password is printed once, or use `--admin-password-file` / `RANSIBLE_ADMIN_PASSWORD`). The UI is embedded in the executable; data lives in SQLite under `--data-dir`.

- **Objects:** projects (git sync or local path), inventories (INI/YAML or dynamic script), credentials (SSH key/password/become/vault; encrypted at rest with a per-install master key, write-only in the API), job templates (extra vars, limit, tags, check/diff, become), jobs (live output over SSE, per-host summary, cancel, relaunch), users with roles `admin` / `operator` (launch, sync, cancel) / `viewer` (read-only), dashboard.
- **API:** `/api/v2/` with AWX-style resources and pagination (`count/next/previous/results`, `?page`, `?page_size`, `?search`, `?order_by`); Bearer tokens (`POST /api/v2/auth/login/`) or HTTP Basic. Launch: `POST /api/v2/job_templates/{id}/launch/`. Live output: `GET /api/v2/jobs/{id}/stream/` (SSE).
- **Isolation:** each job runs the same binary as a subprocess with a minimal environment; credentials are written to a 0700 per-job directory (0600 files) that is deleted afterwards.
- **HTTPS:** `--tls-cert cert.pem --tls-key key.pem` serves TLS directly (rustls); without them the server warns when bound to a non-loopback address. Behind a reverse proxy, plain HTTP on loopback is fine.
- Lean CLI-only build: `cargo build --release --no-default-features`.
- **Schedules** (cron, UTC), **notifications** (webhook / Slack on start, success, failure; URL stored encrypted), **activity stream** (audit trail of changes and launches).
- **Workflows:** DAGs of job templates with success / failure / always edges and approval gates; graph editor and a live status view in the UI; failures handled by a failure edge do not fail the workflow.
- **Surveys:** typed launch-time prompts per template (text, password, integer, float, multiple choice/select; required, defaults, ranges). Password answers are stored encrypted and never returned.
- **Teams and object-level access:** grant a user or team `execute` (launch/sync/cancel) or `admin` (edit/delete) on a specific template, workflow, project, inventory, credential or schedule, on top of the global admin/operator/viewer role. Reads are open to every authenticated user.
- **Remote workers:** `ransible worker --server https://host --token T` turns any machine into an execution node. Create the worker under Workers (token shown once), set a template's "Run on" to it, and its jobs run there: the worker long-polls for work, downloads a bundle with the project and the job's credentials, runs the playbook locally and streams events back (live output, cancel and relaunch work as usual). The worker refuses plain `http://` to non-loopback servers unless `--allow-insecure-http`; a worker that goes silent for 2 minutes fails its job. Cancelling kills the runner process, not grandchildren it already spawned.
- **SSO via a trusted reverse proxy (OIDC / LDAP / SAML through oauth2-proxy, Authelia, Keycloak gatekeeper, ...):** `--proxy-auth-header X-Forwarded-User [--proxy-groups-header X-Forwarded-Groups --proxy-admin-group G --proxy-operator-group G2] [--proxy-trusted-ip IP...]`. The identity header is honoured only from trusted peer IPs (default: loopback), accounts are created on first sight with no usable password, and groups map to admin / operator / viewer on every request. A request that carries a Bearer token or Basic credentials is authenticated by those instead. Make sure clients cannot reach the server except through the proxy.
- Not yet: built-in OIDC/LDAP clients, per-object read restrictions (see plan).

## Platforms
Release binaries (GitHub Releases, with SHA256SUMS and build provenance): Linux x86_64/aarch64 (glibc and static musl), macOS x86_64/aarch64, Windows x86_64/aarch64 (SSH controller; the `local` connection needs a POSIX `sh`). Targets must pass `make check-targets`.

## Vault
`ransible vault encrypt|decrypt|view|rekey|encrypt_string --vault-password-file pw`; encrypted vars files, `!vault` values and encrypted `copy`/`template` sources are decrypted transparently (`--vault-password-file`, `--ask-vault-pass`, `ANSIBLE_VAULT_PASSWORD_FILE`). Files interoperate with `ansible-vault`.

## Development
`make check` (fmt, clippy, tests), `make e2e` (differential vs real ansible on podman), `make security` (cargo-audit, cargo-deny). See `AGENTS.md`, `COMPATIBILITY.md`.

## Testing
`cargo test` runs fixtures in `tests/fixtures` on the local connection. Fixtures were also checked
against real `ansible-playbook` (identical resulting files; recap counters differ slightly for `include_tasks`).
