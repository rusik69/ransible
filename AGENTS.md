# AGENTS.md

Guidance for AI coding agents (and humans) working on **ransible**: an Ansible-compatible
automation runner in Rust. Goal: run existing Ansible inventories/playbooks/roles unchanged.

## Layout
- `src/main.rs` CLI (clap): `playbook`, `adhoc`, `inventory`.
- `src/inventory.rs` INI/YAML inventory, group/host vars, host patterns.
- `src/playbook.rs` YAML -> `Play`/`Task`/`Role` models (k=v args, `with_*`, keyword inheritance).
- `src/template.rs` Jinja2 via minijinja + Ansible filters/tests/lookups.
- `src/executor.rs` run engine: vars, tasks, loops, blocks, handlers, includes, roles, strategy.
- `src/modules.rs` built-in modules (one fn per module, dispatched in `run`).
- `src/conn.rs` local/ssh connection (shells out to `ssh`; `become` via sudo).
- `tests/integration.rs` fast offline tests; `tests/e2e/` differential tests vs real Ansible.

## Commands (see `Makefile`)
- `make check` fmt + clippy (`-D warnings`) + tests. Run before every commit.
- `make e2e` / `make e2e-case CASE=nginx` run ransible and `ansible-playbook` on fresh
  podman containers and compare recap, remote state and idempotence. Needs `podman` and `ansible-playbook`.
- `make security` cargo-audit + cargo-deny.

## Conventions
- Compatibility is measured, not assumed: any behaviour change should be justified by an e2e case or a test.
- Modules must be idempotent, report `changed` accurately, and honour `check`/`diff`.
- Unsupported features must fail loudly (`unsupported module 'x'`), never silently no-op.
- Keep third-party roles out of git (`tests/e2e/cases/*/setup.sh` clones them; they are ignored).
- Never commit secrets, vault passwords or private keys (see `.gitignore`).
- Roadmap: `COMPATIBILITY.md` (status) and the plan in `~/.claude/plans/` if present.
