.PHONY: build build-linux test lint clean

build:
	cargo build --release

build-linux:
	cargo build --release --target x86_64-unknown-linux-gnu

test:
	cargo test

lint:
	cargo clippy -- -D warnings

clean:
	cargo clean
