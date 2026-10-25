INSTALL_DIR := $(HOME)/.local/bin
CONFIG_DIR  := $(HOME)/.config/awpfel
DATA_DIR    := $(HOME)/.local/share/awpfel

.PHONY: dev release sync restart test lint clean

# Dev: debug build + sync + restart daemon (fastest iteration)
dev: sync-bin-debug sync restart
	@echo "\033[0;32m[dev]\033[0m ready — debug build"

# Release: optimized build + sync + restart daemon
release: sync-bin-release sync restart
	@echo "\033[0;32m[release]\033[0m ready — release build"

sync-bin-debug:
	cargo build
	@mkdir -p $(INSTALL_DIR)
	cp target/debug/awpfel $(INSTALL_DIR)/awpfel
	@if [ "$$(uname)" = "Darwin" ]; then \
		xattr -cr $(INSTALL_DIR)/awpfel 2>/dev/null; \
		codesign -fs - $(INSTALL_DIR)/awpfel 2>/dev/null; \
	fi

sync-bin-release:
	cargo build --release
	@mkdir -p $(INSTALL_DIR)
	cp target/release/awpfel $(INSTALL_DIR)/awpfel
	@if [ "$$(uname)" = "Darwin" ]; then \
		xattr -cr $(INSTALL_DIR)/awpfel 2>/dev/null; \
		codesign -fs - $(INSTALL_DIR)/awpfel 2>/dev/null; \
	fi

sync:
	@mkdir -p $(CONFIG_DIR)
	for module in awpfel.zsh awpfel-utils.zsh awpfel-render.zsh awpfel-interact.zsh awpfel-communicate.zsh; do \
		cp plugin/$$module $(CONFIG_DIR)/; \
	done

restart:
	@-$(INSTALL_DIR)/awpfel stop 2>/dev/null; true
	@sleep 0.2
	@$(INSTALL_DIR)/awpfel start &
	@sleep 0.5
	@echo "\033[0;32m[daemon]\033[0m restarted"

# Run all tests
test:
	cargo test
	shellcheck scripts/build-archives.sh
	@if command -v zsh >/dev/null 2>&1; then \
		zsh tests/zsh_smoke_test.zsh; \
	fi

# Lint
lint:
	cargo clippy
	cargo fmt --check
	shellcheck scripts/build-archives.sh

status:
	$(INSTALL_DIR)/awpfel status

logs:
	$(INSTALL_DIR)/awpfel logs -l 30

# Clean build artifacts
clean:
	cargo clean
