# Eo9 — simple entry points. Run `make help` (or just `make`) to see what's here.
#
# These are thin wrappers over `cargo xtask …` (see xtask/src/main.rs); they exist so a
# fresh checkout needs exactly two commands: `make setup`, then `make shell` / `make www` /
# `make qemu`. Each target checks for the host tools it needs and points at `make setup`
# instead of dying with a raw spawn error.

.DEFAULT_GOAL := help
.PHONY: help setup shell www www-build qemu ci

help:
	@echo "Eo9 — common entry points:"
	@echo "  make setup      install/verify prerequisites (Rust targets, wasm-tools; checks QEMU)"
	@echo "  make shell      build the components and drop into the eosh shell on your host"
	@echo "  make www        serve the website + in-browser demos at http://127.0.0.1:8080/"
	@echo "  make www-build  rebuild the /try and /vm demo assets from source, then serve"
	@echo "  make qemu       boot the bare-metal kernel in QEMU to an eosh prompt (aarch64)"
	@echo "  make ci         run the full local gate (host + guest + kernel workspaces)"

setup:
	@command -v rustup >/dev/null 2>&1 || { \
	  echo "error: rustup not found — install it from https://rustup.rs and re-run 'make setup'"; exit 1; }
	rustup target add wasm32-unknown-unknown
	@command -v wasm-tools >/dev/null 2>&1 || cargo install --locked wasm-tools
	@echo ""
	@echo "Prerequisite summary:"
	@command -v rustup >/dev/null 2>&1 \
	  && echo "  ok       rustup (the pinned nightly + per-workspace targets install on first build)" \
	  || echo "  MISSING  rustup — https://rustup.rs"
	@rustup target list --installed 2>/dev/null | grep -q '^wasm32-unknown-unknown$$' \
	  && echo "  ok       wasm32-unknown-unknown target" \
	  || echo "  MISSING  wasm32-unknown-unknown target (rustup target add wasm32-unknown-unknown)"
	@command -v wasm-tools >/dev/null 2>&1 \
	  && echo "  ok       wasm-tools" \
	  || echo "  MISSING  wasm-tools (cargo install --locked wasm-tools)"
	@command -v qemu-system-aarch64 >/dev/null 2>&1 \
	  && echo "  ok       qemu-system-aarch64" \
	  || { echo "  optional qemu-system-aarch64 not found — only needed for 'make qemu'; install it with"; \
	       echo "           your package manager (e.g. 'brew install qemu' / 'apt install qemu-system-arm')"; }

# `make shell` uses a repo-local store (target/eo9-store) so the session always matches the
# components that were just built, and never collides with an older ~/.eo9 store from a
# previously installed binary.
shell:
	@command -v wasm-tools >/dev/null 2>&1 || { \
	  echo "error: wasm-tools not found — run 'make setup' first"; exit 1; }
	cargo xtask build-guest
	EO9_STORE=$(CURDIR)/target/eo9-store cargo run -p eo9

www:
	@echo "Serving the committed site (incl. the /try and /vm demos) at http://127.0.0.1:8080/  (Ctrl-C to stop)"
	cd www && cargo run

www-build:
	@command -v wasm-tools >/dev/null 2>&1 || { \
	  echo "error: wasm-tools not found — run 'make setup' first"; exit 1; }
	cargo xtask build-web-demo
	cargo xtask build-web-vm
	$(MAKE) www

qemu:
	@command -v qemu-system-aarch64 >/dev/null 2>&1 || { \
	  echo "error: qemu-system-aarch64 not found — install QEMU (e.g. 'brew install qemu'), then re-run"; exit 1; }
	@command -v wasm-tools >/dev/null 2>&1 || { \
	  echo "error: wasm-tools not found — run 'make setup' first"; exit 1; }
	cargo xtask build-kernel aarch64
	cargo xtask qemu aarch64

ci:
	@command -v wasm-tools >/dev/null 2>&1 || { \
	  echo "error: wasm-tools not found — run 'make setup' first"; exit 1; }
	cargo xtask ci
