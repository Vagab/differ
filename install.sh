#!/usr/bin/env bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_PREFIX="${INSTALL_PREFIX:-${HOME}/.local}"
INSTALL_DIR="${INSTALL_PREFIX}/bin"
BINARY_NAME="differ"

# Check for cargo
if ! command -v cargo &> /dev/null; then
    echo "Error: cargo (Rust) is not installed."
    echo ""
    echo "Install Rust with:"
    echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    echo ""
    echo "Then restart your shell and run this script again."
    exit 1
fi

echo "Installing differ..."
cd "${SCRIPT_DIR}"
cargo install --path . --root "${INSTALL_PREFIX}" --force

echo "Installed to ${INSTALL_DIR}/${BINARY_NAME}"

echo ""
echo "Done."
echo ""
echo "If '${INSTALL_DIR}' is not on your PATH, add it:"
echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
echo ""
echo "Optional git aliases:"
echo "  git config --global alias.d \"!${INSTALL_DIR}/${BINARY_NAME} diff\""
echo "  git config --global alias.ds \"!${INSTALL_DIR}/${BINARY_NAME} diff --staged\""
