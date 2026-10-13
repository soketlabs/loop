#!/usr/bin/env bash
set -euo pipefail

APP=loop
REPO="soketlabs/loop"
INSTALL_DIR="${LOOP_INSTALL_DIR:-$HOME/.loop/bin}"
RELEASES_URL="https://github.com/${REPO}/releases"
API_URL="https://api.github.com/repos/${REPO}/releases"
TOTAL_STEPS=6

if [ -t 2 ] && [ -z "${NO_COLOR:-}" ]; then
    MUTED=$'\033[0;2m'
    RED=$'\033[0;31m'
    GREEN=$'\033[0;32m'
    ORANGE=$'\033[38;5;214m'
    NC=$'\033[0m'
else
    MUTED='' RED='' GREEN='' ORANGE='' NC=''
fi

usage() {
    cat <<EOF
Loop Installer (macOS, Linux, WSL; use install.ps1 for native Windows)

Usage:
    ./install.sh [options]
    curl -fsSL https://loop.soket.ai/install | bash
    curl -fsSL https://loop.soket.ai/install | bash -s -- --version 0.3.2

Options:
    -h, --help              Display this help message
    -v, --version <version> Install a specific version (e.g., 0.3.2 or v0.3.2)
    -b, --binary <path>     Install from a local binary instead of downloading
        --checksum <hash>   Required SHA-256 for --binary
    -f, --force             Reinstall even if the requested version is already installed
        --no-modify-path    Don't modify shell config files (.zshrc, .bashrc, etc.)

Environment:
    LOOP_INSTALL_DIR        Install directory (default: ~/.loop/bin)
    LOOP_VERSION            Same as --version
    GITHUB_TOKEN            Used for GitHub API calls (avoids rate limits)
    NO_COLOR                Disable colored output
EOF
}

requested_version="${LOOP_VERSION:-}"
no_modify_path=false
force=false
binary_path=""
binary_checksum=""

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
                echo "${RED}Error: --version requires a version argument${NC}" >&2
                exit 1
            fi
            ;;
        -b|--binary)
            if [[ -n "${2:-}" ]]; then
                binary_path="$2"
                shift 2
            else
                echo "${RED}Error: --binary requires a path argument${NC}" >&2
                exit 1
            fi
            ;;
        -f|--force)
            force=true
            shift
            ;;
        --checksum)
            [[ -n "${2:-}" ]] || { echo "--checksum requires a SHA-256 hash" >&2; exit 1; }
            binary_checksum="$2"
            shift 2
            ;;
        --no-modify-path)
            no_modify_path=true
            shift
            ;;
        *)
            echo "${RED}Error: unknown option '$1'${NC}" >&2
            usage >&2
            exit 1
            ;;
    esac
done

# ---------------------------------------------------------------- output ----
# Everything goes to stderr so `curl ... | bash` never feeds text to a shell.

info()  { printf '%s\n' "${MUTED}$*${NC}" >&2; }
ok()    { printf '%s\n' "  ${GREEN}✓${NC} $*" >&2; }
warn()  { printf '%s\n' "  ${ORANGE}!${NC} $*" >&2; }
fail()  { printf '%s\n' "  ${RED}✗${NC} $*" >&2; }
step()  { printf '\n%s\n' "${ORANGE}[$1/${TOTAL_STEPS}]${NC} $2" >&2; }

die() {
    fail "$1"
    shift
    local hint
    for hint in "$@"; do info "    $hint"; done
    exit 1
}

human_size() {
    awk -v b="${1:-0}" 'BEGIN {
        if (b >= 1048576) printf "%.1f MB", b / 1048576;
        else if (b >= 1024) printf "%.0f KB", b / 1024;
        else printf "%d B", b;
    }'
}

# ------------------------------------------------------------------ curl ----
# No overall --max-time: a big download on a slow link is fine as long as it
# keeps moving. We only abort when throughput stays under 1 KB/s for 20s.
CURL_COMMON=(--connect-timeout 10 --speed-limit 1024 --speed-time 20 --retry 3 --retry-delay 1 --retry-connrefused)
# Filled in only if the first attempt fails (HTTP/2 / IPv6 stalls are common
# on HPC login nodes and corporate proxies).
CURL_EXTRA=()

curl_fast() {
    curl "${CURL_COMMON[@]}" ${CURL_EXTRA[@]+"${CURL_EXTRA[@]}"} "$@"
}

github_api() {
    local args=(-fsSL -H "Accept: application/vnd.github+json" -H "X-GitHub-Api-Version: 2022-11-28")
    if [[ -n "${GITHUB_TOKEN:-}" ]]; then
        args+=(-H "Authorization: Bearer ${GITHUB_TOKEN}")
    fi
    curl_fast "${args[@]}" "$@"
}

# Download one file. On a TTY curl draws its own progress bar. When piped
# (curl | bash, CI) we poll the file size so the user still sees movement.
_download_once() {
    local url="$1" out="$2" pid rc=0 size start=$SECONDS elapsed

    if [ -t 2 ]; then
        curl_fast -fL --progress-bar -o "$out" "$url" >&2 || rc=$?
        printf '\r\033[K' >&2
        return "$rc"
    fi

    curl_fast -fsSL -o "$out" "$url" &
    pid=$!
    while kill -0 "$pid" 2>/dev/null; do
        sleep 2
        kill -0 "$pid" 2>/dev/null || break
        size=0
        if [ -f "$out" ]; then
            size=$(( $(wc -c < "$out" 2>/dev/null || echo 0) ))
        fi
        elapsed=$(( SECONDS - start ))
        info "    ... $(human_size "$size") downloaded (${elapsed}s)"
    done
    wait "$pid" || rc=$?
    return "$rc"
}

download_file() {
    local url="$1" out="$2"
    if ! _download_once "$url" "$out"; then
        warn "Download failed; retrying over HTTP/1.1 + IPv4"
        CURL_EXTRA=(--http1.1 -4)
        rm -f "$out"
        _download_once "$url" "$out"
    fi
}

# -------------------------------------------------------------- platform ----

OS="" ARCH="" LIBC="" IS_WSL=false TARGET="" EXT="tar.gz" BIN_NAME="$APP"

detect_platform() {
    local raw_os raw_arch
    raw_os=$(uname -s)
    raw_arch=$(uname -m)

    case "$raw_os" in
        Darwin*) OS="darwin" ;;
        Linux*)
            OS="linux"
            if grep -qi microsoft /proc/version 2>/dev/null; then IS_WSL=true; fi
            ;;
        MINGW*|MSYS*|CYGWIN*) OS="windows"; EXT="zip"; BIN_NAME="${APP}.exe" ;;
        *) die "Unsupported operating system: ${raw_os}" "Supported releases: ${RELEASES_URL}" ;;
    esac

    case "$raw_arch" in
        aarch64|arm64) ARCH="aarch64" ;;
        x86_64|amd64) ARCH="x86_64" ;;
        *) die "Unsupported architecture: ${raw_arch}" "Supported releases: ${RELEASES_URL}" ;;
    esac

    # An x86_64 shell running under Rosetta on Apple Silicon: use the native build.
    if [ "$OS" = "darwin" ] && [ "$ARCH" = "x86_64" ]; then
        if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
            ARCH="aarch64"
        fi
    fi

    case "${OS}-${ARCH}" in
        linux-*)
            if ldd --version 2>&1 | grep -qi musl || ls /lib/ld-musl-* >/dev/null 2>&1; then
                die "musl-based Linux (e.g. Alpine) is not supported: releases are built for glibc only." \
                    "Build from source instead: cargo install --path crates/loop-cli" \
                    "Releases: ${RELEASES_URL}"
            fi
            LIBC="gnu"
            TARGET="${ARCH}-unknown-linux-gnu"
            ;;
        darwin-*) TARGET="${ARCH}-apple-darwin" ;;
        windows-x86_64) TARGET="x86_64-pc-windows-msvc" ;;
        *) die "Unsupported platform: ${OS}/${ARCH}" "Supported releases: ${RELEASES_URL}" ;;
    esac
}

# Fast path: GitHub redirects /releases/latest to /releases/tag/<tag>. That is a
# single HEAD request, needs no API token and is not subject to API rate limits.
latest_tag() {
    local url tag=""
    url=$(curl_fast -sIL -o /dev/null -w '%{url_effective}' "${RELEASES_URL}/latest" 2>/dev/null || true)
    if [[ "$url" == */releases/tag/* ]]; then
        tag="${url##*/releases/tag/}"
    else
        local json
        json=$(github_api "${API_URL}/latest" 2>/dev/null || true)
        tag=$(printf '%s' "$json" | sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
    fi
    printf '%s' "$tag"
}

installed_version_of() {
    local bin="$1" v
    [ -x "$bin" ] || return 0
    v=$("$bin" --version 2>/dev/null) || return 1
    v=$(printf '%s\n' "$v" | head -n 1)
    v="${v##* }"
    printf '%s' "${v#v}"
}

# --------------------------------------------------------------- checksum ----

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print tolower($1)}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print tolower($1)}'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$1" | awk '{print tolower($NF)}'
    else
        return 1
    fi
}

verify_checksum() {
    local archive="$1" checksum_file="$2" expected actual
    expected=$(awk 'NR==1 {print tolower($1)}' "$checksum_file" | tr -d '[:space:]')
    [[ "$expected" =~ ^[a-f0-9]{64}$ ]] || die "Checksum file did not contain a valid SHA-256 hash"
    [ "$(awk 'NF {n++} END {print n+0}' "$checksum_file")" = 1 ] || die "Checksum file must contain one entry"
    [ "$(awk 'NF {print NF}' "$checksum_file")" -le 2 ] || die "Checksum file contains extra fields"
    local checksum_name
    checksum_name=$(awk '{print $2}' "$checksum_file")
    [ -z "$checksum_name" ] || [ "${checksum_name#\*}" = "$(basename "$archive")" ] || die "Checksum names a different asset"

    if ! actual=$(sha256_of "$archive"); then
        die "No SHA-256 tool found (sha256sum/shasum/openssl); refusing installation"
    fi

    if [ "$expected" != "$actual" ]; then
        die "Checksum mismatch for $(basename "$archive")" "expected ${expected}" "got      ${actual}"
    fi
    ok "Checksum verified (sha256 ${actual:0:12}...)"
}

# ---------------------------------------------------------------- install ----

# Copy to a temp name in the destination directory, then rename. The rename is
# atomic and works even if the old binary is currently running.
place_binary() {
    local src="$1" dest="$INSTALL_DIR/$BIN_NAME" staged candidate_version new_hash old_hash old_version backup
    staged=$(mktemp "$INSTALL_DIR/.${BIN_NAME}.new.XXXXXX")
    STAGED_PATH="$staged"
    cp "$src" "$staged"
    chmod 755 "$staged"
    candidate_version=$(installed_version_of "$staged") || die "Staged binary failed its version check; current binary kept"
    [[ "$candidate_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][A-Za-z0-9.-]+)?$ ]] || die "Staged binary failed its version check; current binary kept"
    if [ -n "${tag:-}" ]; then
        [ "$candidate_version" = "${tag#v}" ] || die "Staged binary version does not match ${tag}; current binary kept"
    fi
    new_hash=$(sha256_of "$staged") || die "Cannot verify staged binary"
    backup=""
    if [ -L "$STATE_DIR/rollback.json" ] || [ -L "$STATE_DIR/rollback.json.new" ] || [ -L "$STATE_DIR/rollback.json.saved" ]; then
        die "Rollback records must not be symlinks"
    fi
    [ ! -f "$STATE_DIR/rollback.json" ] || cp "$STATE_DIR/rollback.json" "$STATE_DIR/rollback.json.saved"
    if [ -f "$dest" ]; then
        [ ! -L "$dest" ] || die "Refusing to replace a symlink"
        old_version=$(installed_version_of "$dest") || old_version=""
        if [[ "$old_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][A-Za-z0-9.-]+)?$ ]]; then
            old_hash=$(sha256_of "$dest") || die "Cannot verify previous binary"
            backup="$STATE_DIR/previous-${old_hash}"
            [ "$OS" != windows ] || backup="${backup}.exe"
            if [ -e "$backup" ] || [ -L "$backup" ]; then
                if [ ! -f "$backup" ] || [ -L "$backup" ]; then
                    die "Previous binary must be a regular file"
                fi
                [ "$(sha256_of "$backup")" = "$old_hash" ] || die "Previous binary checksum mismatch"
            else
                cp "$dest" "$backup"
            fi
            printf '{"schema":1,"current_sha256":"%s","previous_sha256":"%s","previous_version":"%s"}\n' "$new_hash" "$old_hash" "$old_version" > "$STATE_DIR/rollback.json.new"
            mv -f "$STATE_DIR/rollback.json.new" "$STATE_DIR/rollback.json"
        else
            warn "Repairing an unusable installation; no rollback record will be created"
            rm -f "$STATE_DIR/rollback.json"
        fi
    fi
    if ! mv -f "$staged" "$dest"; then
        if [ -f "$STATE_DIR/rollback.json.saved" ]; then
            mv -f "$STATE_DIR/rollback.json.saved" "$STATE_DIR/rollback.json"
        else
            rm -f "$STATE_DIR/rollback.json"
        fi
        die "Cannot replace binary; close running Loop processes and retry"
    fi
    local installed
    if ! installed=$(installed_version_of "$dest") || [ "$installed" != "$candidate_version" ]; then
        if [ -n "$backup" ]; then
            cp "$backup" "$staged"
            chmod 755 "$staged"
            mv -f "$staged" "$dest"
            if [ -f "$STATE_DIR/rollback.json.saved" ]; then
                mv -f "$STATE_DIR/rollback.json.saved" "$STATE_DIR/rollback.json"
            else
                rm -f "$STATE_DIR/rollback.json"
            fi
            die "Installed binary failed its final check; previous binary restored from $backup"
        fi
        die "Installed binary failed its final check; rerun the installer to restore a checked release"
    fi
    rm -f "$STATE_DIR/rollback.json.saved"
    STAGED_PATH=""
}

TMP_DIR=""
STATE_DIR="" LOCK_HELD=false STAGED_PATH=""
cleanup() {
    [ -z "$TMP_DIR" ] || rm -rf "$TMP_DIR"
    [ -z "$STAGED_PATH" ] || rm -f "$STAGED_PATH"
    [ "$LOCK_HELD" != true ] || rmdir "$STATE_DIR/operation.lock"
    return 0
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

download_and_install() {
    local tag="$1" archive_name url checksum_url archive extracted
    local start=$SECONDS

    archive_name="${APP}-${TARGET}.${EXT}"
    url="${RELEASES_URL}/download/${tag}/${archive_name}"
    checksum_url="${url}.sha256"

    TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/loop_install.XXXXXX")
    archive="$TMP_DIR/$archive_name"

    step 3 "Downloading ${archive_name}"
    info "    ${url}"

    # Fetch the tiny checksum file in parallel with the archive.
    local ck_pid
    curl_fast -fsSL -o "${archive}.sha256" "$checksum_url" 2>/dev/null &
    ck_pid=$!

    download_file "$url" "$archive" || die "Failed to download ${archive_name}" \
        "Check that a build exists for ${TARGET}: ${RELEASES_URL}"
    ok "Downloaded $(human_size "$(wc -c < "$archive")") in $(( SECONDS - start ))s"

    step 4 "Verifying integrity"
    if wait "$ck_pid" && [ -s "${archive}.sha256" ]; then
        verify_checksum "$archive" "${archive}.sha256"
    else
        die "No checksum found at ${checksum_url}" \
            "Refusing to install an unverified binary."
    fi

    step 5 "Installing to ${INSTALL_DIR}"
    info "    Extracting..."
    extracted="$TMP_DIR/$BIN_NAME"
    if [ "$EXT" = tar.gz ]; then
        tar -xzOf "$archive" "${APP}-${TARGET}/${BIN_NAME}" > "$extracted" || die "Archive did not contain ${BIN_NAME}"
    else
        unzip -p "$archive" "${APP}-${TARGET}/${BIN_NAME}" > "$extracted" || die "Archive did not contain ${BIN_NAME}"
    fi
    place_binary "$extracted"
    ok "Installed ${INSTALL_DIR}/${BIN_NAME}"
}

install_from_binary() {
    step 3 "Installing from local binary"
    [ -f "$binary_path" ] || die "Binary not found at ${binary_path}"
    step 4 "Verifying integrity"
    [[ "$binary_checksum" =~ ^[a-fA-F0-9]{64}$ ]] || die "--binary requires --checksum with a SHA-256 hash"
    local actual
    actual=$(sha256_of "$binary_path") || die "No SHA-256 tool found; refusing installation"
    [ "$actual" = "$(printf '%s' "$binary_checksum" | tr 'A-F' 'a-f')" ] || die "Local binary checksum mismatch"
    step 5 "Installing to ${INSTALL_DIR}"
    place_binary "$binary_path"
    ok "Installed ${INSTALL_DIR}/${BIN_NAME}"
}

# ------------------------------------------------------------------- PATH ----

add_to_path() {
    local config_file="$1" command="$2"

    if grep -Fxq "$command" "$config_file"; then
        ok "PATH already configured in ${config_file}"
    elif [[ -w "$config_file" ]]; then
        printf '\n# loop\n%s\n' "$command" >> "$config_file"
        ok "Added loop to PATH in ${config_file}"
    else
        warn "Manually add the directory to ${config_file} (or similar):"
        info "    $command"
    fi
}

configure_path() {
    step 6 "Configuring PATH"

    if [[ "$no_modify_path" == "true" ]]; then
        info "    skipped (--no-modify-path)"
        return
    fi
    if [[ ":$PATH:" == *":$INSTALL_DIR:"* ]]; then
        ok "PATH already contains ${INSTALL_DIR}"
        return
    fi

    local xdg="${XDG_CONFIG_HOME:-$HOME/.config}"
    local zd="${ZDOTDIR:-$HOME}"
    local current_shell config_files file config_file=""
    current_shell=$(basename "${SHELL:-bash}")

    case "$current_shell" in
        fish) config_files="$HOME/.config/fish/config.fish" ;;
        zsh)  config_files="$zd/.zshrc $zd/.zshenv $xdg/zsh/.zshrc $xdg/zsh/.zshenv" ;;
        bash) config_files="$HOME/.bashrc $HOME/.bash_profile $HOME/.profile $xdg/bash/.bashrc $xdg/bash/.bash_profile" ;;
        ash|sh) config_files="$HOME/.ashrc $HOME/.profile /etc/profile" ;;
        *)    config_files="$HOME/.bashrc $HOME/.bash_profile $xdg/bash/.bashrc $xdg/bash/.bash_profile" ;;
    esac

    if [ "$OS" = "windows" ]; then
        warn "Windows support is still in testing."
        info "    This only puts loop on PATH inside Git Bash; add ${INSTALL_DIR} to your Windows PATH for PowerShell/cmd."
    fi

    for file in $config_files; do
        if [[ -f "$file" ]]; then config_file="$file"; break; fi
    done

    if [[ -z "$config_file" ]]; then
        warn "No config file found for ${current_shell}. Add this to your PATH manually:"
        info "    export PATH=$INSTALL_DIR:\$PATH"
        return
    fi

    local quoted_install_dir
    printf -v quoted_install_dir '%q' "$INSTALL_DIR"
    case "$current_shell" in
        fish) add_to_path "$config_file" "fish_add_path $quoted_install_dir" ;;
        zsh|bash|ash|sh) add_to_path "$config_file" "export PATH=$quoted_install_dir:\$PATH" ;;
        *)
            warn "Manually add the directory to ${config_file} (or similar):"
            info "    export PATH=$INSTALL_DIR:\$PATH"
            ;;
    esac
}

print_done() {
    local label="$1" dest="$INSTALL_DIR/$BIN_NAME"

    {
        echo ""
        echo "${ORANGE}  ██╗      ██████╗  ██████╗ ██████╗ ${NC}"
        echo "${ORANGE}  ██║     ██╔═══██╗██╔═══██╗██╔══██╗${NC}"
        echo "${ORANGE}  ██║     ██║   ██║██║   ██║██████╔╝${NC}"
        echo "${ORANGE}  ██║     ██║   ██║██║   ██║██╔═══╝ ${NC}"
        echo "${ORANGE}  ███████╗╚██████╔╝╚██████╔╝██║     ${NC}"
        echo "${ORANGE}  ╚══════╝ ╚═════╝  ╚═════╝ ╚═╝     ${NC}"
        echo ""
        echo "  ${GREEN}✓${NC} ${MUTED}loop ${NC}${label} ${MUTED}installed in $(( SECONDS ))s${NC}"
        echo "  ${MUTED}→${NC} ${dest}"
        echo ""
        echo "  cd <project>   ${MUTED}# open a directory${NC}"
        echo "  loop           ${MUTED}# start the coding agent${NC}"
        echo ""
        if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
            echo "  ${MUTED}Restart your shell, or run:${NC}"
            echo "    export PATH=$INSTALL_DIR:\$PATH"
            echo ""
        fi
        echo "  ${MUTED}Docs: ${NC}https://loop.soket.ai"
        echo ""
    } >&2
}

# ------------------------------------------------------------------- main ----

info ""
info "Loop installer"

step 1 "Detecting your system"
detect_platform
if [ "$IS_WSL" = "true" ]; then
    ok "WSL (Linux on Windows) on ${ARCH} -> using the Linux build"
fi
ok "${OS}/${ARCH}${LIBC:+ (${LIBC})} -> ${TARGET}"

mkdir -p "$INSTALL_DIR"
STATE_DIR="$INSTALL_DIR/.loop-update"
[ ! -L "$STATE_DIR" ] || die "Update state directory must not be a symlink"
mkdir -p "$STATE_DIR"
mkdir "$STATE_DIR/operation.lock" 2>/dev/null || die "Another installation operation is running" "After a crash, remove $STATE_DIR/operation.lock only when no installer or updater is running."
LOCK_HELD=true
dest_bin="$INSTALL_DIR/$BIN_NAME"
[ ! -L "$dest_bin" ] || die "Refusing to replace a symlink"
if [ -e "$dest_bin" ]; then [ -f "$dest_bin" ] || die "Install destination is not a regular file"; fi
current="$(installed_version_of "$dest_bin")" || current=""
[ -z "$current" ] || info "    currently installed: ${current}"

if [ -n "$binary_path" ]; then
    step 2 "Skipping release lookup (--binary)"
    install_from_binary
    label="local"
else
    step 2 "Finding the release"
    if [ -n "$requested_version" ]; then
        tag="v${requested_version#v}"
        ok "Requested version ${tag}"
    else
        info "    querying GitHub for the latest release..."
        tag=$(latest_tag)
        if [ -n "$tag" ]; then
            ok "Latest release is ${tag}"
        else
            die "Could not resolve a release version; retry or use --version"
        fi
    fi
    label="${tag#v}"
    [[ "$label" =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][A-Za-z0-9.-]+)?$ ]] || die "Invalid release version"

    if [ -n "$tag" ] && [ "$current" = "${tag#v}" ] && [ "$force" != "true" ]; then
        ok "Already up to date (${current}); use --force to reinstall"
        step 3 "Download"; info "    skipped"
        step 4 "Verifying integrity"; info "    skipped"
        step 5 "Install"; info "    skipped"
    else
        download_and_install "$tag"
    fi
fi

configure_path

if [ -n "${GITHUB_ACTIONS-}" ] && [ "${GITHUB_ACTIONS}" = "true" ] && [ -n "${GITHUB_PATH-}" ]; then
    echo "$INSTALL_DIR" >> "$GITHUB_PATH"
    ok "Added ${INSTALL_DIR} to \$GITHUB_PATH"
fi

print_done "$label"
