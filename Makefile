.PHONY: build deploy clean

UNAME_S := $(shell uname -s)

# Build everything and deploy to Raycast extension (macOS only)
deploy: build
	@echo "Deploying to Raycast extension..."
	@cp target/release/findr raycast-extension/assets/findr
ifeq ($(UNAME_S),Darwin)
	@cp findr-ocr/.build/release/findr-ocr raycast-extension/assets/findr-ocr
	@codesign --force --sign - raycast-extension/assets/findr
	@codesign --force --sign - raycast-extension/assets/findr-ocr
endif
	@echo "Done. Restart Raycast to pick up new binaries."

# Build Rust binary (+ Swift on macOS)
build:
	cargo build --release
ifeq ($(UNAME_S),Darwin)
	cd findr-ocr && swift build -c release
endif

clean:
	cargo clean
	rm -rf findr-ocr/.build
