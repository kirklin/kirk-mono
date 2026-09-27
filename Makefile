.PHONY: fonts clean

fonts:
	cargo run --release --quiet --manifest-path tools/Cargo.toml

clean:
	rm -rf build fonts tools/target
