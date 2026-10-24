SHELL := /bin/bash
TARGET := x86_64-unknown-linux-musl
DIST := dist
VERSION := $(shell grep -m1 '^version' Cargo.toml | cut -d'"' -f2)

.PHONY: all build test fmt clippy package clean run-server run-agent

all: test build

build:
	cargo build --release --target $(TARGET)

test:
	cargo test --workspace

fmt:
	cargo fmt --all

clippy:
	cargo clippy --workspace --all-targets

package: build
	@mkdir -p $(DIST)/bin $(DIST)/install
	install -m 755 target/$(TARGET)/release/ytmon-agent $(DIST)/bin/
	install -m 755 target/$(TARGET)/release/ytmon-server $(DIST)/bin/
	install -m 755 target/$(TARGET)/release/ytmon-dist $(DIST)/bin/
	install -m 755 packaging/dist/install/*.sh $(DIST)/install/
	@cd $(DIST)/bin && sha256sum ytmon-* > ../SHA256SUMS
	@echo "packaged $(VERSION) -> $(DIST)/"

clean:
	cargo clean
	rm -rf $(DIST)

run-server:
	cargo run -p ytmon-server -- --listen 127.0.0.1:19180 --db /tmp/ytmon.db

run-agent:
	cargo run -p ytmon-agent -- --config ./agent.toml
