#!/usr/bin/env bash
set -e

INSTALL_DIR="${HOME}/.local/bin"
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

# Offer to set up git aliases
echo ""
read -p "Set up git aliases? (git d, git ds) [y/N] " -n 1 -r
echo ""
if [[ $REPLY =~ ^[Yy]$ ]]; then
    GITCONFIG="${HOME}/.gitconfig"

    # Check if aliases already exist
    if grep -q "^\s*d\s*=.*differ" "$GITCONFIG" 2>/dev/null; then
        echo "Git aliases already configured."
    else
        # Append alias section (git merges duplicate sections)
        {
            echo ""
            echo "[alias]"
            printf '\td = ! %s diff\n' "${INSTALL_DIR}/${BINARY_NAME}"
            printf '\tds = ! %s diff --staged\n' "${INSTALL_DIR}/${BINARY_NAME}"
        } >> "$GITCONFIG"

        echo "Added git aliases:"
        echo "  git d   -> differ diff"
        echo "  git ds  -> differ diff --staged"
    fi
else
    echo "Skipped. Add manually to ~/.gitconfig:"
    echo "  [alias]"
    echo "      d = ! ${INSTALL_DIR}/${BINARY_NAME} diff"
    echo "      ds = ! ${INSTALL_DIR}/${BINARY_NAME} diff --staged"
fi
