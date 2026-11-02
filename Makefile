BIN_NAME       := niri-screenshot-daemon
TARGET_RELEASE := target/release/$(BIN_NAME)

CARGO   ?= cargo
INSTALL ?= install

.PHONY: all build check test clippy fmt fmt-check install clean

all: build

## Compilation
build:
	$(CARGO) build --release

check:
	$(CARGO) check --workspace

## Strict testing & clippy pedantic
test:
	$(CARGO) test --workspace
	$(CARGO) clippy --workspace --all-targets --all-features -- -D warnings -D clippy::all -D clippy::pedantic

clippy:
	$(CARGO) clippy --workspace --all-targets --all-features -- -D warnings -D clippy::all -D clippy::pedantic

fmt:
	$(CARGO) fmt

fmt-check:
	$(CARGO) fmt --check

## Installation: copies binary to current user's $HOME/bin, and to /home/sosa/bin, /home/prtesting/bin
install: build
	@echo "==> Installing for current user ($(USER)) to $(HOME)/bin..."
	$(INSTALL) -Dm755 $(TARGET_RELEASE) $(HOME)/bin/$(BIN_NAME)
	@echo "==> Staging binary to /tmp for multi-user distribution..."
	@TMP_BIN=$$(mktemp /tmp/$(BIN_NAME).XXXXXX) && \
		$(INSTALL) -m755 $(TARGET_RELEASE) "$$TMP_BIN" && \
		echo "==> Installing for user sosa to /home/sosa/bin..." && \
		sudo -u sosa $(INSTALL) -Dm755 "$$TMP_BIN" /home/sosa/bin/$(BIN_NAME) && \
		echo "==> Installing for user prtesting to /home/prtesting/bin..." && \
		sudo -u prtesting $(INSTALL) -Dm755 "$$TMP_BIN" /home/prtesting/bin/$(BIN_NAME) && \
		rm -f "$$TMP_BIN"
	@echo "==> Verifying installations:"
	@ls -l $(HOME)/bin/$(BIN_NAME)
	@sudo -u sosa ls -l /home/sosa/bin/$(BIN_NAME)
	@sudo -u prtesting ls -l /home/prtesting/bin/$(BIN_NAME)
	@echo "==> Installation complete."

clean:
	$(CARGO) clean

