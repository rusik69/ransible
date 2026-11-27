.PHONY: check-targets dist all build release test e2e e2e-case fmt fmt-check lint audit deny security check clean install ci
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

# Type-check every release target locally (no linker needed)
check-targets:
	@for t in $(TARGETS); do rustup target add $$t >/dev/null 2>&1; echo "== $$t"; $(CARGO) check -q --target $$t || exit 1; done

# Build a release archive for the host platform into dist/
dist: release
	@mkdir -p dist && tar -C target/release -czf dist/rancible-$$(uname -s | tr A-Z a-z)-$$(uname -m).tar.gz rancible && ls -l dist
