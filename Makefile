APP       := luddite
VERSION   := $(shell grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
RELEASE   := ../_shared/releases/$(APP)

# Binary name
BIN       := luddite

# Detect host platform
UNAME_S   := $(shell uname -s)
UNAME_M   := $(shell uname -m)

ifeq ($(UNAME_S),Linux)
  HOST_PLATFORM := linux
  ifeq ($(UNAME_M),aarch64)
    MUSL_TARGET := aarch64-unknown-linux-musl
    GNU_TARGET  := aarch64-unknown-linux-gnu
  else
    MUSL_TARGET := x86_64-unknown-linux-musl
    GNU_TARGET  := x86_64-unknown-linux-gnu
  endif
  # Static musl binary when the toolchain is present (portable across distros)
  HAS_MUSL := $(shell command -v musl-gcc >/dev/null 2>&1 && echo yes)
  ifeq ($(HAS_MUSL),yes)
    HOST_TARGET := $(MUSL_TARGET)
  else
    HOST_TARGET := $(GNU_TARGET)
  endif
endif
ifeq ($(UNAME_S),Darwin)
  HOST_PLATFORM := mac
  HOST_TARGET   := $(UNAME_M)-apple-darwin
endif
ifeq ($(OS),Windows_NT)
  HOST_PLATFORM := windows
  HOST_TARGET   := x86_64-pc-windows-msvc
endif

EXT       := $(if $(filter windows,$(HOST_PLATFORM)),.exe,)
DIST      := dist
CARGO     := cargo

# Honour a shared CARGO_TARGET_DIR when one is set
TARGET_DIR := $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),target)

BIN_DIR    := $(HOME)/.local/bin
CONFIG_DIR := $(HOME)/.config/luddite

.PHONY: all build release dev check test clippy fmt verify smoke \
        build-static install uninstall clean dist-clean help

all: build

# ─── Development ──────────────────────────────────────────────

dev: ## Fast debug build + list the loaded rule set
	$(CARGO) build
	@echo ""
	$(TARGET_DIR)/debug/$(BIN)$(EXT) --list-patterns

check: ## Type-check without building
	$(CARGO) check

test: ## Run all tests
	$(CARGO) test

clippy: ## Run clippy linter
	$(CARGO) clippy -- -D warnings

fmt: ## Check formatting
	$(CARGO) fmt -- --check

verify: clippy test fmt ## Run all checks (clippy + test + fmt)

smoke: build ## Build a fixture repo with every known fingerprint, run --guarantee, expect VERIFIED CLEAN
	scripts/smoke.sh $(DIST)/$(BIN)$(EXT) $(DIST)/smoke

# ─── Build ────────────────────────────────────────────────────

build: ## Release build → dist/
	@echo "Building $(APP) v$(VERSION) for $(HOST_TARGET)..."
	$(CARGO) build --release --target $(HOST_TARGET)
	@mkdir -p $(DIST)
	cp $(TARGET_DIR)/$(HOST_TARGET)/release/$(BIN)$(EXT) $(DIST)/$(BIN)$(EXT)
	@echo ""
	@ls -lh $(DIST)/$(BIN)$(EXT)

build-static: ## Static musl build (requires musl-tools)
	@command -v musl-gcc >/dev/null 2>&1 || \
		{ echo "Error: musl-gcc not found. Install musl-tools: sudo apt install musl-tools"; exit 1; }
	@echo "Building static $(APP) v$(VERSION) for $(MUSL_TARGET)..."
	$(CARGO) build --release --target $(MUSL_TARGET)
	@mkdir -p $(DIST)
	cp $(TARGET_DIR)/$(MUSL_TARGET)/release/$(BIN) $(DIST)/$(BIN)

# ─── Release ──────────────────────────────────────────────────

release: build ## Release build → _shared/releases/luddite/{platform}
	@mkdir -p $(RELEASE)/$(HOST_PLATFORM)
	cp $(DIST)/$(BIN)$(EXT) $(RELEASE)/$(HOST_PLATFORM)/$(BIN)-$(VERSION)-$(HOST_TARGET)$(EXT)
	@echo "Released to $(RELEASE)/$(HOST_PLATFORM)/"
	@ls -lh $(RELEASE)/$(HOST_PLATFORM)/

# ─── Install ──────────────────────────────────────────────────

install: build ## Build and install to ~/.local/bin
	@mkdir -p $(BIN_DIR)
	install -m 0755 $(DIST)/$(BIN)$(EXT) $(BIN_DIR)/$(BIN)$(EXT)
	@echo ""
	@echo "Installed $(BIN_DIR)/$(BIN)$(EXT)  (v$(VERSION), $(HOST_TARGET))"
	@case ":$$PATH:" in *":$(BIN_DIR):"*) ;; \
		*) echo "  NOTE: $(BIN_DIR) is not on your PATH";; esac
	@echo "  Personal rules: $(CONFIG_DIR)/patterns.toml  (optional, merged with built-ins)"
	@echo ""
	@echo "  Try:   $(BIN) --list-patterns"
	@echo "         $(BIN) /path/to/repo --dry-run"
	@echo "         $(BIN) /path/to/repo --guarantee"

# ─── Uninstall ────────────────────────────────────────────────

uninstall: ## Remove the installed binary (preserves personal rules)
	rm -f $(BIN_DIR)/$(BIN)$(EXT)
	@echo "Uninstalled. Personal rules preserved at $(CONFIG_DIR)/ — remove manually if desired."

# ─── Clean ────────────────────────────────────────────────────

clean: ## Remove build artifacts
	$(CARGO) clean
	rm -rf $(DIST)

dist-clean: clean ## Remove dist/ and release outputs
	rm -rf $(RELEASE)/$(HOST_PLATFORM)

# ─── Help ─────────────────────────────────────────────────────

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | sort | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'
