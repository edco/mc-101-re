HOST_TARGET := $(shell rustc -vV | awk '/^host:/ { print $$2 }')

.PHONY: all package test clean

all: package

package:
	cargo run --manifest-path xtask/Cargo.toml --target $(HOST_TARGET) --release

test:
	cargo test --manifest-path shared/Cargo.toml --target $(HOST_TARGET)
	cargo test --manifest-path xtask/Cargo.toml --target $(HOST_TARGET)

clean:
	cargo clean
	cargo clean --manifest-path xtask/Cargo.toml
