#!/usr/bin/env bash
set -e

INSTALL_DIR="${HOME}/.local/bin"
BINARY_NAME="differ"

echo "Building differ..."
cargo build --release

echo "Installing to ${INSTALL_DIR}..."
mkdir -p "${INSTALL_DIR}"
cp "target/release/${BINARY_NAME}" "${INSTALL_DIR}/"
chmod +x "${INSTALL_DIR}/${BINARY_NAME}"

# Function to add to PATH in a shell config file
add_to_path() {
    local file="$1"
    local path_line='export PATH="${HOME}/.local/bin:${PATH}"'

    if [ -f "$file" ]; then
        if ! grep -q '\.local/bin' "$file"; then
            echo "" >> "$file"
            echo "# Added by differ installer" >> "$file"
            echo "$path_line" >> "$file"
            echo "Added PATH to $file"
        else
            echo "PATH already configured in $file"
        fi
    fi
}

# Check if ~/.local/bin is in PATH
if [[ ":$PATH:" != *":${INSTALL_DIR}:"* ]]; then
    echo ""
    echo "Adding ${INSTALL_DIR} to PATH..."

    # Add to .bashrc if it exists
    add_to_path "${HOME}/.bashrc"

    # Add to .zshrc if it exists
    add_to_path "${HOME}/.zshrc"

    echo ""
    echo "Restart your shell or run:"
    echo "  export PATH=\"\${HOME}/.local/bin:\${PATH}\""
else
    echo "PATH already includes ${INSTALL_DIR}"
fi

echo ""
echo "Done! Run 'differ --help' to get started."
echo ""
echo "Optional: Add git aliases to ~/.gitconfig:"
echo "  [alias]"
echo "      d = ! ${INSTALL_DIR}/${BINARY_NAME} diff"
echo "      ds = ! ${INSTALL_DIR}/${BINARY_NAME} diff --staged"
