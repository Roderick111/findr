.PHONY: build deploy clean

# Build everything and deploy to Raycast extension
deploy: build
	@echo "Deploying to Raycast extension..."
	@cp target/release/findr raycast-extension/assets/findr
	@cp findr-ocr/.build/release/findr-ocr raycast-extension/assets/findr-ocr
	@codesign --force --sign - raycast-extension/assets/findr
	@codesign --force --sign - raycast-extension/assets/findr-ocr
	@echo "Done. Restart Raycast to pick up new binaries."

# Build both Rust and Swift binaries
build:
	cargo build --release
	cd findr-ocr && swift build -c release

clean:
	cargo clean
	rm -rf findr-ocr/.build
