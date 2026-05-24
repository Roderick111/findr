FROM rust:latest

WORKDIR /app

# Layer 1: Cache dependency build (only rebuilds when Cargo.toml/lock change)
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main(){}" > src/main.rs && \
    cargo build --release 2>&1 && \
    rm -rf src

# Layer 2: Build actual source (fast rebuild on code changes)
COPY src/ src/
RUN touch src/main.rs && cargo build --release

# Test corpus: text files
RUN mkdir -p /test-corpus/docs /test-corpus/code /test-corpus/images && \
    echo "Hello world from a text file" > /test-corpus/docs/readme.txt && \
    echo "Invoice #1234 for consulting services" > /test-corpus/docs/invoice.txt && \
    echo "fn main() { println!(\"hello\"); }" > /test-corpus/code/main.rs && \
    echo "Meeting notes from Q4 planning session" > /test-corpus/docs/notes.md

# Test corpus: images with text for OCR testing
RUN apt-get update && apt-get install -y --no-install-recommends imagemagick fonts-dejavu-core && \
    convert -size 400x100 xc:white -font DejaVu-Sans -pointsize 24 \
        -draw "text 20,60 'RECEIPT: Total 42.99'" \
        /test-corpus/images/receipt.png && \
    convert -size 400x100 xc:white -font DejaVu-Sans -pointsize 24 \
        -draw "text 20,60 'Property Tax Notice 2025'" \
        /test-corpus/images/tax-notice.png && \
    convert -size 400x100 xc:white -font DejaVu-Sans -pointsize 24 \
        -draw "text 20,60 'Meeting Agenda Q4 Review'" \
        /test-corpus/images/agenda.jpg && \
    apt-get remove -y imagemagick fonts-dejavu-core && apt-get autoremove -y && \
    rm -rf /var/lib/apt/lists/*

ENTRYPOINT ["/app/target/release/findr"]
