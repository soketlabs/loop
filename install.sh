#!/usr/bin/env bash
set -euo pipefail

APP=loop
REPO="soketlabs/loop"
INSTALL_DIR="${LOOP_INSTALL_DIR:-$HOME/.loop/bin}"
RELEASES_URL="https://github.com/${REPO}/releases"
API_URL="https://api.github.com/repos/${REPO}/releases"

MUTED='\033[0;2m'
RED='\033[0;31m'
GREEN='\033[0;32m'
ORANGE='\033[38;5;214m'
NC='\033[0m'

usage() {
    cat <<EOF
Loop Installer

Usage:
    ./install.sh [options]
    bash install.sh [options]
    curl -fsSL https://loop.soket.ai/install | bash

Options:
    -h, --help              Display this help message
    -v, --version <version> Install a specific version (e.g., 0.3.2 or v0.3.2)
    -b, --binary <path>     Install from a local binary instead of downloading
        --no-modify-path    Don't modify shell config files (.zshrc, .bashrc, etc.)

Examples:
    curl -fsSL https://loop.soket.ai/install | bash
    curl -fsSL https://loop.soket.ai/install | bash -s -- --version 0.3.2
    ./install.sh
    ./install.sh --binary /path/to/loop
EOF
}

requested_version=${VERSION:-}
no_modify_path=false
binary_path=""
installed_tag=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help)
            usage
            exit 0
            ;;
        -v|--version)
            if [[ -n "${2:-}" ]]; then
                requested_version="$2"
                shift 2
            else
                echo -e "${RED}Error: --version requires a version argument${NC}" >&2
                exit 1
            fi
            ;;
        -b|--binary)
            if [[ -n "${2:-}" ]]; then
                binary_path="$2"
                shift 2
            else
                echo -e "${RED}Error: --binary requires a path argument${NC}" >&2
                exit 1
            fi
            ;;
        --no-modify-path)
            no_modify_path=true
            shift
            ;;
        *)
            echo -e "${ORANGE}Warning: Unknown option '$1'${NC}" >&2
            shift
            ;;
    esac
done

mkdir -p "$INSTALL_DIR"

print_message() {
    local level=$1
    local message=$2
    local color="$NC"

    case $level in
        error) color="${RED}" ;;
        success) color="${GREEN}" ;;
        warning) color="${ORANGE}" ;;
    esac

    # Always write to stderr so `curl … | bash` and a local `./install.sh`
    # never feed status text (or ANSI codes) into another shell.
    echo -e "${color}${message}${NC}" >&2
}

ok() {
    echo -e "  ${GREEN}✓${NC} $1" >&2
}

fail() {
    echo -e "  ${RED}✗${NC} $1" >&2
}

curl_base() {
    # HTTP/2 and IPv6 stalls are common on HPC login nodes talking to GitHub.
    curl --http1.1 -4 --connect-timeout 15 --max-time 120 --retry 2 --retry-delay 1 "$@"
}

github_curl() {
    local extra=()
    if [[ -n "${GITHUB_TOKEN:-}" ]]; then
        extra=(-H "Authorization: Bearer ${GITHUB_TOKEN}")
    fi
    curl_base -fsSL \
        -H "Accept: application/vnd.github+json" \
        -H "X-GitHub-Api-Version: 2022-11-28" \
        "${extra[@]}" \
        "$@"
}

detect_target() {
    local raw_os os arch

    raw_os=$(uname -s)
    os=$(echo "$raw_os" | tr '[:upper:]' '[:lower:]')
    case "$raw_os" in
        Darwin*) os="darwin" ;;
        Linux*) os="linux" ;;
        MINGW*|MSYS*|CYGWIN*) os="windows" ;;
    esac

    arch=$(uname -m)
    case "$arch" in
        aarch64|arm64) arch="aarch64" ;;
        x86_64|amd64) arch="x86_64" ;;
    esac

    if [ "$os" = "darwin" ] && [ "$arch" = "x86_64" ]; then
        local rosetta_flag
        rosetta_flag=$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)
        if [ "$rosetta_flag" = "1" ]; then
            arch="aarch64"
        fi
    fi

    case "${os}-${arch}" in
        linux-x86_64) echo "x86_64-unknown-linux-gnu" ;;
        linux-aarch64) echo "aarch64-unknown-linux-gnu" ;;
        darwin-x86_64) echo "x86_64-apple-darwin" ;;
        darwin-aarch64) echo "aarch64-apple-darwin" ;;
        windows-x86_64) echo "x86_64-pc-windows-msvc" ;;
        *)
            fail "Unsupported OS/Arch: ${os}/${arch}"
            print_message info "${MUTED}Supported releases: ${NC}${RELEASES_URL}"
            exit 1
            ;;
    esac
}

resolve_version() {
    local tag json

    if [ -n "$requested_version" ]; then
        requested_version="${requested_version#v}"
        echo "v${requested_version}"
        return
    fi

    json=$(github_curl "${API_URL}/latest" || true)
    tag=$(printf '%s' "$json" | sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)

    if [ -z "$tag" ]; then
        fail "Failed to fetch the latest release from GitHub"
        print_message info "${MUTED}See ${NC}${RELEASES_URL}"
        exit 1
    fi

    echo "$tag"
}

installed_binary_name() {
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*) echo "${APP}.exe" ;;
        *) echo "$APP" ;;
    esac
}

check_version() {
    if command -v "$APP" >/dev/null 2>&1; then
        local installed_version
        installed_version=$("$APP" --version 2>/dev/null || echo "")
        installed_version="${installed_version##* }"
        installed_version="${installed_version#v}"
        if [ -n "$installed_version" ]; then
            print_message info "${MUTED}Currently installed: ${NC}${installed_version}"
        fi
    fi
}

download_file() {
    local url="$1"
    local output="$2"
    local quiet="${3:-}"
    local ret

    if [ "$quiet" = "1" ] || ! [ -t 2 ]; then
        curl_base -fsSL -o "$output" "$url"
        return
    fi

    # Progress goes to stderr. Clear the bar when curl finishes so it
    # cannot leave leftover #=#=# characters on the prompt.
    set +e
    curl_base -fL --progress-bar -o "$output" "$url" >&2
    ret=$?
    set -e
    printf '\r\033[K' >&2
    return "$ret"
}

verify_checksum() {
    local archive="$1"
    local checksum_file="$2"
    local expected actual

    expected=$(awk '{print $1}' "$checksum_file" | tr -d '[:space:]' | tr '[:upper:]' '[:lower:]')
    if [ -z "$expected" ]; then
        fail "Checksum file was empty"
        exit 1
    fi

    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$archive" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$archive" | awk '{print $1}')
    else
        print_message warning "No sha256 tool found; skipping checksum verification"
        return 0
    fi

    actual=$(echo "$actual" | tr '[:upper:]' '[:lower:]')
    if [ "$expected" != "$actual" ]; then
        fail "Checksum mismatch for $(basename "$archive")"
        print_message info "${MUTED}expected ${NC}${expected}"
        print_message info "${MUTED}got      ${NC}${actual}"
        exit 1
    fi
}

extract_archive() {
    local archive="$1"
    local dest="$2"

    case "$archive" in
        *.tar.gz)
            if ! command -v tar >/dev/null 2>&1; then
                fail "'tar' is required but not installed."
                exit 1
            fi
            tar -xzf "$archive" -C "$dest"
            ;;
        *.zip)
            if command -v unzip >/dev/null 2>&1; then
                unzip -q "$archive" -d "$dest"
            elif command -v tar >/dev/null 2>&1; then
                tar -xf "$archive" -C "$dest"
            else
                fail "'unzip' is required to install the Windows archive."
                exit 1
            fi
            ;;
        *)
            fail "Unknown archive format: $archive"
            exit 1
            ;;
    esac
}

find_extracted_binary() {
    local dest="$1"
    local name="$2"
    local found

    found=$(find "$dest" -type f -name "$name" | head -n 1)
    if [ -z "$found" ]; then
        fail "Extracted archive did not contain ${name}"
        exit 1
    fi
    echo "$found"
}

download_and_install() {
    local tag="$1"
    local target="$2"
    local installed_binary="$3"
    local version="${tag#v}"
    local ext archive_name url checksum_url tmp_dir extracted

    if [[ "$target" == *windows* ]]; then
        ext="zip"
    else
        ext="tar.gz"
    fi

    archive_name="${APP}-${target}.${ext}"
    url="${RELEASES_URL}/download/${tag}/${archive_name}"
    checksum_url="${url}.sha256"

    print_message info ""
    print_message info "${MUTED}Installing ${NC}${APP} ${MUTED}version ${NC}${version} ${MUTED}for ${NC}${target}"

    tmp_dir="${TMPDIR:-/tmp}/loop_install_$$"
    mkdir -p "$tmp_dir"
    trap 'rm -rf "$tmp_dir"' EXIT

    print_message info "${MUTED}Downloading ${NC}${archive_name}${MUTED} (timeout 120s)${NC}"
    print_message info "${MUTED}${url}${NC}"
    if ! download_file "$url" "$tmp_dir/$archive_name"; then
        fail "Failed to download ${archive_name}"
        print_message info "${MUTED}Available releases: ${NC}${RELEASES_URL}"
        exit 1
    fi
    ok "Downloaded"

    if download_file "$checksum_url" "$tmp_dir/${archive_name}.sha256" 1; then
        verify_checksum "$tmp_dir/$archive_name" "$tmp_dir/${archive_name}.sha256"
        ok "Checksum verified"
    else
        print_message warning "Could not download checksum; continuing without verification"
    fi

    extract_archive "$tmp_dir/$archive_name" "$tmp_dir"
    extracted=$(find_extracted_binary "$tmp_dir" "$installed_binary")
    mv "$extracted" "$INSTALL_DIR/$installed_binary"
    chmod 755 "$INSTALL_DIR/$installed_binary"
    ok "Installed to ${INSTALL_DIR}/${installed_binary}"
    rm -rf "$tmp_dir"
    trap - EXIT
}

install_from_binary() {
    local installed_binary
    installed_binary=$(installed_binary_name)

    print_message info ""
    print_message info "${MUTED}Installing ${NC}${APP} ${MUTED}from ${NC}${binary_path}"
    if [ ! -f "$binary_path" ]; then
        fail "Binary not found at ${binary_path}"
        exit 1
    fi
    cp "$binary_path" "$INSTALL_DIR/$installed_binary"
    chmod 755 "$INSTALL_DIR/$installed_binary"
    ok "Installed to ${INSTALL_DIR}/${installed_binary}"
}

add_to_path() {
    local config_file=$1
    local command=$2

    if grep -Fxq "$command" "$config_file"; then
        ok "PATH already configured in ${config_file}"
    elif [[ -w $config_file ]]; then
        echo -e "\n# loop" >> "$config_file"
        echo "$command" >> "$config_file"
        ok "Added loop to PATH in ${config_file}"
    else
        print_message warning "Manually add the directory to ${config_file} (or similar):"
        print_message info "  $command"
    fi
}

print_done() {
    local dest="$INSTALL_DIR/$(installed_binary_name)"
    local version_label="${installed_tag:-local}"
    version_label="${version_label#v}"

    echo "" >&2
    echo -e "${ORANGE}  ██╗      ██████╗  ██████╗ ██████╗ ${NC}" >&2
    echo -e "${ORANGE}  ██║     ██╔═══██╗██╔═══██╗██╔══██╗${NC}" >&2
    echo -e "${ORANGE}  ██║     ██║   ██║██║   ██║██████╔╝${NC}" >&2
    echo -e "${ORANGE}  ██║     ██║   ██║██║   ██║██╔═══╝ ${NC}" >&2
    echo -e "${ORANGE}  ███████╗╚██████╔╝╚██████╔╝██║     ${NC}" >&2
    echo -e "${ORANGE}  ╚══════╝ ╚═════╝  ╚═════╝ ╚═╝     ${NC}" >&2
    echo "" >&2
    echo -e "  ${GREEN}✓${NC} ${MUTED}loop ${NC}${version_label} ${MUTED}installed${NC}" >&2
    echo -e "  ${MUTED}→${NC} ${dest}" >&2
    echo "" >&2
    echo -e "  cd <project>   ${MUTED}# open a directory${NC}" >&2
    echo -e "  loop           ${MUTED}# start the coding agent${NC}" >&2
    echo "" >&2
    if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
        echo -e "  ${MUTED}Restart your shell, or run:${NC}" >&2
        echo -e "    export PATH=$INSTALL_DIR:\$PATH" >&2
        echo "" >&2
    fi
    echo -e "  ${MUTED}Docs: ${NC}https://github.com/${REPO}" >&2
    echo "" >&2
}

if [ -n "$binary_path" ]; then
    installed_tag="local"
    install_from_binary
else
    os_target=$(detect_target)
    specific_tag=$(resolve_version)
    installed_tag="$specific_tag"
    installed_binary=$(installed_binary_name)
    check_version
    download_and_install "$specific_tag" "$os_target" "$installed_binary"
fi

XDG_CONFIG_HOME=${XDG_CONFIG_HOME:-$HOME/.config}
current_shell=$(basename "${SHELL:-bash}")
case $current_shell in
    fish)
        config_files="$HOME/.config/fish/config.fish"
        ;;
    zsh)
        config_files="${ZDOTDIR:-$HOME}/.zshrc ${ZDOTDIR:-$HOME}/.zshenv $XDG_CONFIG_HOME/zsh/.zshrc $XDG_CONFIG_HOME/zsh/.zshenv"
        ;;
    bash)
        config_files="$HOME/.bashrc $HOME/.bash_profile $HOME/.profile $XDG_CONFIG_HOME/bash/.bashrc $XDG_CONFIG_HOME/bash/.bash_profile"
        ;;
    ash|sh)
        config_files="$HOME/.ashrc $HOME/.profile /etc/profile"
        ;;
    *)
        config_files="$HOME/.bashrc $HOME/.bash_profile $XDG_CONFIG_HOME/bash/.bashrc $XDG_CONFIG_HOME/bash/.bash_profile"
        ;;
esac

if [[ "$no_modify_path" != "true" ]]; then
    config_file=""
    for file in $config_files; do
        if [[ -f $file ]]; then
            config_file=$file
            break
        fi
    done

    if [[ -z $config_file ]]; then
        print_message warning "No config file found for $current_shell. You may need to manually add to PATH:"
        print_message info "  export PATH=$INSTALL_DIR:\$PATH"
    elif [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
        case $current_shell in
            fish)
                add_to_path "$config_file" "fish_add_path $INSTALL_DIR"
                ;;
            zsh|bash|ash|sh)
                add_to_path "$config_file" "export PATH=$INSTALL_DIR:\$PATH"
                ;;
            *)
                print_message warning "Manually add the directory to $config_file (or similar):"
                print_message info "  export PATH=$INSTALL_DIR:\$PATH"
                ;;
        esac
    else
        ok "PATH already contains ${INSTALL_DIR}"
    fi
fi

if [ -n "${GITHUB_ACTIONS-}" ] && [ "${GITHUB_ACTIONS}" = "true" ] && [ -n "${GITHUB_PATH-}" ]; then
    echo "$INSTALL_DIR" >> "$GITHUB_PATH"
    ok "Added ${INSTALL_DIR} to \$GITHUB_PATH"
fi

print_done
