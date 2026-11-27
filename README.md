# rancible

Ansible-compatible automation runner in Rust. Single binary, runs existing Ansible
inventories, playbooks and roles unchanged. No Python required on controller or targets.

```
cargo build --release
rancible playbook site.yml -i inventory.ini [-l web] [-t tag] [--skip-tags x] [-e k=v|@file] [-C] [-D] [-v] [-f 5]
rancible playbook site.yml --list-tasks | --list-hosts | --syntax-check
rancible adhoc web -i inventory -m shell -a 'uptime'
rancible inventory 'web:&prod' -i inventory
```

## Supported
- **Inventory**: INI + YAML, groups/children/vars, `host_vars/` `group_vars/`, ranges `web[01:05]`, patterns (`a:b`, `&`, `!`, globs, `~regex`), `ansible_host/port/user/connection/ssh_private_key_file/become*`.
- **Playbooks**: plays, `vars`, `vars_files`, pre/post tasks, roles (+`meta` deps, defaults, vars, templates, files), handlers (`notify`/`listen`), blocks (`rescue`/`always`), `include_tasks`/`import_tasks`/`include_role`/`import_role`/`import_playbook`, `when`, `loop`/`with_items`/`with_list`/`with_dict`, `loop_control`, `register`, `tags`, `become`, `ignore_errors`, `changed_when`/`failed_when`, `until`/`retries`/`delay`, `delegate_to`, `run_once`, `serial`, `environment`, `check_mode`, `no_log`, `meta`.
- **Templating**: Jinja2 (minijinja) + Ansible filters/tests/lookups (`default`, `to_json`, `to_nice_yaml`, `regex_replace`, `combine`, `dict2items`, `b64encode`, `mandatory`, `ternary`, `version`, `lookup('env'|'file'|'pipe')` ...), native types for `{{ expr }}`.
- **Modules** (short or `ansible.builtin.` names): command shell raw script copy template file lineinfile blockinfile replace stat slurp debug set_fact include_vars fail assert ping pause wait_for wait_for_connection setup apt yum dnf package apk homebrew service systemd user group git get_url uri fetch synchronize meta.
- **Connections**: `local`, `ssh` (system `ssh`, ControlMaster; key/agent auth, `sudo` become without password).

## Not yet supported
Galaxy collections / non-builtin modules (clear "unsupported module" error), Vault, dynamic inventory scripts/plugins, `free` strategy, become passwords, `async`/`poll`, `copy` of directories, `with_fileglob/subelements/nested`, callbacks, `ansible_facts` beyond the basic set from `setup`.

## Platforms
Release binaries (GitHub Releases, with SHA256SUMS and build provenance): Linux x86_64/aarch64 (glibc and static musl), macOS x86_64/aarch64, Windows x86_64/aarch64 (SSH controller; the `local` connection needs a POSIX `sh`). Targets must pass `make check-targets`.

## Vault
`rancible vault encrypt|decrypt|view|rekey|encrypt_string --vault-password-file pw`; encrypted vars files, `!vault` values and encrypted `copy`/`template` sources are decrypted transparently (`--vault-password-file`, `--ask-vault-pass`, `ANSIBLE_VAULT_PASSWORD_FILE`). Files interoperate with `ansible-vault`.

## Development
`make check` (fmt, clippy, tests), `make e2e` (differential vs real ansible on podman), `make security` (cargo-audit, cargo-deny). See `AGENTS.md`, `COMPATIBILITY.md`.

## Testing
`cargo test` runs fixtures in `tests/fixtures` on the local connection. Fixtures were also checked
against real `ansible-playbook` (identical resulting files; recap counters differ slightly for `include_tasks`).
