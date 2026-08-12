VERSION := $(shell grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
NAME := kkagent
TARGETS := \
	x86_64-apple-darwin \
	aarch64-apple-darwin \
	x86_64-unknown-linux-gnu \
	aarch64-unknown-linux-gnu \
	x86_64-unknown-linux-musl \
	aarch64-unknown-linux-musl \
	x86_64-pc-windows-msvc \
	aarch64-pc-windows-msvc

RELEASE_DIR := target/release-dist

.PHONY: build test release clean help

help:
	@echo "kkagent build system"
	@echo ""
	@echo "  make build     - Debug build (current platform)"
	@echo "  make release   - Release build (current platform)"
	@echo "  make test      - Run all tests"
	@echo "  make dist      - Build eight release targets (requires their SDKs/linkers)"
	@echo "  make clean     - Clean build artifacts"

build:
	cargo build

release:
	cargo build --release

test:
	cargo test

dist: $(TARGETS)
	@echo "Built for all targets in $(RELEASE_DIR)/"

$(TARGETS):
	@echo "Building for $@..."
	cargo build --release --target $@
	@mkdir -p $(RELEASE_DIR)
	@if echo "$@" | grep -q windows; then \
		cp target/$@/release/$(NAME).exe $(RELEASE_DIR)/$(NAME)-$(VERSION)-$@.exe 2>/dev/null || true; \
	else \
		cp target/$@/release/$(NAME) $(RELEASE_DIR)/$(NAME)-$(VERSION)-$@ 2>/dev/null || true; \
	fi

clean:
	cargo clean
	rm -rf $(RELEASE_DIR)

install: release
	cp target/release/$(NAME) /usr/local/bin/$(NAME)
	@echo "Installed $(NAME) to /usr/local/bin/"

lint:
	cargo clippy --all-targets -- -D warnings

fmt:
	cargo fmt --all -- --check

fmt-fix:
	cargo fmt --all
