.PHONY: ui-smoke check-targets dist all build release test e2e e2e-case fmt fmt-check lint audit deny security check clean install ci
CARGO ?= cargo
all: build

build:
	$(CARGO) build

release:
	$(CARGO) build --release

test:
	$(CARGO) test --all-targets

# Differential tests vs real ansible-playbook (needs podman + ansible-playbook)
e2e: build
	tests/e2e/run.sh

# make e2e-case CASE=nginx
e2e-case: build
	tests/e2e/run.sh $(CASE)

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

lint:
	$(CARGO) clippy --all-targets -- -D warnings

audit:
	$(CARGO) audit

deny:
	$(CARGO) deny check

security: audit deny

check: fmt-check lint test

ci: check security

install:
	$(CARGO) install --path .

clean:
	$(CARGO) clean
	rm -rf tests/e2e/.work dist

TARGETS ?= x86_64-unknown-linux-gnu x86_64-unknown-linux-musl aarch64-unknown-linux-gnu aarch64-unknown-linux-musl x86_64-apple-darwin aarch64-apple-darwin x86_64-pc-windows-msvc aarch64-pc-windows-msvc

# Type-check every release target locally (CLI-only: the server feature bundles SQLite, a C build that needs a
# target C toolchain; CI builds the full binary natively per target)
check-targets:
	@for t in $(TARGETS); do rustup target add $$t >/dev/null 2>&1; echo "== $$t"; $(CARGO) check -q --no-default-features --target $$t || exit 1; done

# Build a release archive for the host platform into dist/
dist: release
	@mkdir -p dist && tar -C target/release -czf dist/ransible-$$(uname -s | tr A-Z a-z)-$$(uname -m).tar.gz ransible && ls -l dist

# Smoke-test the embedded web UI in jsdom (needs node + `npm install` in tests/ui)
ui-smoke: build
	@d=$$(mktemp -d); mkdir -p $$d/proj; printf -- '- hosts: all\n  gather_facts: no\n  tasks:\n    - debug: msg=hi\n' > $$d/proj/site.yml; echo smoke-pass-123 > $$d/pw; \
	$(CARGO) run -q -- serve --listen 127.0.0.1:18199 --data-dir $$d/data --admin-password-file $$d/pw >/dev/null 2>&1 & pid=$$!; sleep 3; \
	(cd tests/ui && BASE=http://127.0.0.1:18199 ADMIN_PW=smoke-pass-123 node smoke.mjs $$d/proj); rc=$$?; kill $$pid 2>/dev/null; rm -rf $$d; exit $$rc
