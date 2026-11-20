#!/bin/sh
# Install bk (Beankeeper CLI) from GitHub Releases.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Govcraft/beankeeper/main/install.sh | sh
#
# Options (environment variables):
#   BK_VERSION   - version to install (default: latest)
#   BK_INSTALL   - install directory (default: /usr/local/bin)

set -eu

REPO="Govcraft/beankeeper"
INSTALL_DIR="${BK_INSTALL:-/usr/local/bin}"

main() {
    detect_platform
    resolve_version
    download_and_install
    verify
}

detect_platform() {
    OS="$(uname -s)"
    ARCH="$(uname -m)"

    case "$OS" in
        Linux)  OS_TARGET="unknown-linux-gnu" ;;
        Darwin) OS_TARGET="apple-darwin" ;;
        *)      fatal "unsupported OS: $OS" ;;
    esac

    case "$ARCH" in
        x86_64|amd64)  ARCH_TARGET="x86_64" ;;
        aarch64|arm64) ARCH_TARGET="aarch64" ;;
        *)             fatal "unsupported architecture: $ARCH" ;;
    esac

    TARGET="${ARCH_TARGET}-${OS_TARGET}"
}

resolve_version() {
    if [ -n "${BK_VERSION:-}" ]; then
        VERSION="$BK_VERSION"
        return
    fi

    info "fetching latest version..."
    TAG="$(curl -fsSI -o /dev/null -w '%{redirect_url}' \
        "https://github.com/${REPO}/releases/latest" 2>/dev/null \
        | grep -oE '[^/]+$')"

    if [ -z "$TAG" ]; then
        fatal "could not determine latest release"
    fi

    VERSION="${TAG#beankeeper-cli-v}"
}

download_and_install() {
    ARCHIVE="bk-${VERSION}-${TARGET}.tar.gz"
    URL="https://github.com/${REPO}/releases/download/beankeeper-cli-v${VERSION}/${ARCHIVE}"
    CHECKSUM_URL="https://github.com/${REPO}/releases/download/beankeeper-cli-v${VERSION}/checksums-sha256.txt"

    TMPDIR="$(mktemp -d)"
    trap 'rm -rf "$TMPDIR"' EXIT

    info "downloading bk v${VERSION} for ${TARGET}..."
    curl -fsSL "$URL" -o "${TMPDIR}/${ARCHIVE}"

    info "verifying checksum..."
    curl -fsSL "$CHECKSUM_URL" -o "${TMPDIR}/checksums-sha256.txt"

    verify_signatures

    EXPECTED="$(awk -v archive="$ARCHIVE" '$2 == archive {print $1}' "${TMPDIR}/checksums-sha256.txt")"
    if [ -z "$EXPECTED" ]; then
        fatal "no checksum found for ${ARCHIVE}"
    fi

    ACTUAL="$(compute_sha256 "${TMPDIR}/${ARCHIVE}")"
    if [ "$ACTUAL" != "$EXPECTED" ]; then
        fatal "checksum mismatch: expected ${EXPECTED}, got ${ACTUAL}"
    fi

    tar xzf "${TMPDIR}/${ARCHIVE}" -C "$TMPDIR"

    if [ -w "$INSTALL_DIR" ]; then
        install -m755 "${TMPDIR}/bk-${VERSION}-${TARGET}/bk" "${INSTALL_DIR}/bk"
    else
        info "installing to ${INSTALL_DIR} (requires sudo)..."
        sudo install -m755 "${TMPDIR}/bk-${VERSION}-${TARGET}/bk" "${INSTALL_DIR}/bk"
    fi
}

verify_signatures() {
    if ! command -v cosign >/dev/null 2>&1; then
        warn "cosign not found: only the SHA-256 checksum will be checked; release authenticity is not verified"
        return
    fi

    IDENTITY="https://github.com/${REPO}/.github/workflows/release.yml@refs/tags/beankeeper-cli-v${VERSION}"
    for ARTIFACT in "$ARCHIVE" checksums-sha256.txt; do
        info "verifying Sigstore bundle for ${ARTIFACT}..."
        curl -fsSL "${URL%/*}/${ARTIFACT}.sigstore" -o "${TMPDIR}/${ARTIFACT}.sigstore" \
            || fatal "could not download Sigstore bundle for ${ARTIFACT}"
        cosign verify-blob \
            --bundle "${TMPDIR}/${ARTIFACT}.sigstore" \
            --certificate-identity "$IDENTITY" \
            --certificate-oidc-issuer https://token.actions.githubusercontent.com \
            "${TMPDIR}/${ARTIFACT}" \
            || fatal "Sigstore verification failed for ${ARTIFACT}"
    done
}

compute_sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        fatal "no sha256sum or shasum found"
    fi
}

verify() {
    INSTALLED_VERSION="$("${INSTALL_DIR}/bk" --version)" || fatal "installed bk could not be executed"
    if [ "$INSTALLED_VERSION" != "bk ${VERSION}" ]; then
        fatal "installed binary reports '${INSTALLED_VERSION}', expected 'bk ${VERSION}'"
    fi

    info "installed bk v${VERSION} to ${INSTALL_DIR}/bk"
    if [ "$(command -v bk || true)" != "${INSTALL_DIR}/bk" ]; then
        warn "add ${INSTALL_DIR} to the front of your PATH to use this installation"
    fi
}

info() { printf '  \033[1;32m>\033[0m %s\n' "$1"; }
warn() { printf '  \033[1;33m>\033[0m %s\n' "$1" >&2; }
fatal() { printf '  \033[1;31merror:\033[0m %s\n' "$1" >&2; exit 1; }

main
