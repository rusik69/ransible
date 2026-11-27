# Compatibility with Ansible

Status is *measured*: `make e2e` runs each case with real `ansible-playbook` and with `ransible`
against fresh systemd containers (podman) and compares changed/failed/unreachable counts,
remote state and idempotence (second run = 0 changes).

## e2e matrix (tests/e2e/cases)
| Case | Role / playbook | Status |
|---|---|---|
| app | local role w/ dependency, template, handlers, git, become_user | PASS |
| git-pip | geerlingguy.git, geerlingguy.pip | PASS |
| nginx | geerlingguy.nginx | PASS |
| security-ntp | geerlingguy.security, geerlingguy.ntp | PASS |
| apache, redis, postgresql, memcached, haproxy, java, nodejs, php, docker, certbot, pip-venv | geerlingguy roles | PASS |
| mysql | geerlingguy.mysql (native mysql_db/mysql_user via `mysql` CLI) | PASS |

Offline tests (`cargo test`) additionally pin variable precedence to real Ansible output
(`tests/fixtures/prec`).

## Core features
| Area | Status |
|---|---|
| INI / YAML inventory, groups, ranges, patterns, group_vars/host_vars (inventory & playbook dir) | done |
| Dynamic inventory scripts (`--list`, `_meta.hostvars`) | done |
| Variable precedence (defaults < inventory < facts < play vars < vars_files < role vars < task vars < set_fact < role params < extra-vars) | done |
| Roles (deps, defaults, vars, handlers, tasks_from), include/import tasks & roles, blocks, handlers/listen, tags | done |
| Loops: loop, with_items/list/dict/first_found/fileglob/nested/subelements/together/sequence/indexed_items | done |
| `when`, `register`, `until`, `changed_when`, `failed_when`, `delegate_to`, `run_once`, `serial`, `environment`, `omit` | done |
| Jinja2 + Ansible filters/tests/lookups (core set) | done (see `src/template.rs`) |
| Ansible Vault 1.1/1.2: encrypted files, inline `!vault`, `vault` subcommand; interoperable with `ansible-vault` | done |
| SSH + local connections, sudo become | done |
| `async`/`poll: 0` + `async_status`, `max_fail_percentage`, `any_errors_fatal`, rescue vars (`ansible_failed_task/result`) | done |
| `ansible.cfg` + `ANSIBLE_*` env, SSH password (needs `sshpass`) and become password, `docker`/`podman` connections, `-k/-K/-T/--start-at-task/--list-tags`, `ansible-playbook`/`ansible`/`ansible-vault` argv0 names, shell completions | done |
| `strategy: free` (each host runs the play at its own pace; `hostvars` of other hosts are not visible during the play), `add_host`, `group_by` | done |
| Non-role collections/Galaxy collections, vault ids | not yet |
| AWX-style web UI + REST API in the same binary (`ransible serve`): projects, inventories, credentials, templates, jobs with live output, users/roles, dashboard | done incl. schedules, notifications, activity stream, workflows with approvals, surveys, teams and object-level grants (OIDC/LDAP, remote workers: planned) |

## Modules (native)
command shell raw script copy template file lineinfile blockinfile replace stat slurp find debug set_fact include_vars fail assert ping pause wait_for wait_for_connection meta setup service_facts package_facts getent apt yum dnf package apk homebrew apt_repository apt_key deb822_repository pip service systemd user group authorized_key cron timezone hostname locale_gen tempfile unarchive git get_url uri fetch synchronize mysql_db mysql_user mysql_query mysql_variables async_status ini_file sysctl known_hosts reboot set_stats assemble archive npm gem debconf snap ufw firewalld yum_repository rpm_key alternatives mount modprobe selinux git_config.

Anything else fails with `unsupported module 'x'` (never a silent no-op).
