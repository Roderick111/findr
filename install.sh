#!/bin/bash
# Findr installer — downloads pre-built binary from GitHub Releases
set -e

REPO="Roderick111/findr"
INSTALL_DIR="$HOME/.local/bin"

# Detect OS and architecture
OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
    Darwin)
        case "$ARCH" in
            arm64|aarch64) ASSET="findr-macos-arm64" ;;
            x86_64)        ASSET="findr-macos-x86_64" ;;
            *)             echo "Unsupported architecture: $ARCH"; exit 1 ;;
        esac
        ;;
    Linux)
        case "$ARCH" in
            x86_64)        ASSET="findr-linux-x86_64" ;;
            aarch64|arm64) ASSET="findr-linux-arm64" ;;
            *)             echo "Unsupported architecture: $ARCH"; exit 1 ;;
        esac
        ;;
    *)
        echo "Unsupported OS: $OS"
        echo "For Windows, use install.ps1 or: cargo install --git https://github.com/$REPO.git"
        exit 1
        ;;
esac

echo "Installing findr for $OS/$ARCH..."

# Get latest release URL
DOWNLOAD_URL=$(curl -s "https://api.github.com/repos/$REPO/releases/latest" \
    | grep "browser_download_url.*$ASSET" \
    | cut -d '"' -f 4)

if [ -z "$DOWNLOAD_URL" ]; then
    echo "No pre-built binary found for $OS/$ARCH. Falling back to cargo install..."
    if ! command -v cargo &> /dev/null; then
        echo "Rust not installed. Installing..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        source "$HOME/.cargo/env"
    fi
    cargo install --git "https://github.com/$REPO.git"
    echo "Installed to $(which findr)"
    echo ""
    echo "Run: findr search \"test\" (first run auto-builds the index)"
    exit 0
fi

# Download and install
mkdir -p "$INSTALL_DIR"
echo "Downloading from $DOWNLOAD_URL..."
curl -sL "$DOWNLOAD_URL" -o "$INSTALL_DIR/findr"
chmod +x "$INSTALL_DIR/findr"

# Add to PATH if needed
if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
    SHELL_RC="$HOME/.zshrc"
    [ -f "$HOME/.bashrc" ] && SHELL_RC="$HOME/.bashrc"
    echo "export PATH=\"$INSTALL_DIR:\$PATH\"" >> "$SHELL_RC"
    export PATH="$INSTALL_DIR:$PATH"
    echo "Added $INSTALL_DIR to PATH (restart shell or run: source $SHELL_RC)"
fi

echo "Installed findr to $INSTALL_DIR/findr"
echo ""
echo "Run: findr search \"test\" (first run auto-builds the index)"
