#!/bin/sh
# Rad installer for Linux and macOS.
#
#   curl -fsSL https://radengine.dev/install.sh | sh
#
# Environment:
#   RAD_VERSION   install a specific version tag (default: latest release)
#   RAD_INSTALL   install directory (default: ~/.rad)
#   RAD_REPO      GitHub repo to download from (default: Southclaws/rad)
#
# Installs the single `rad` binary — the database server, the devtool, and
# the codegen CLI in one.
set -eu

main() {
	repo="${RAD_REPO:-Southclaws/rad}"
	install_dir="${RAD_INSTALL:-$HOME/.rad}"
	bin_dir="$install_dir/bin"

	case "$(uname -s)" in
	Darwin) os="darwin" ;;
	Linux) os="linux" ;;
	*)
		echo "error: unsupported OS $(uname -s) — on Windows, use install.ps1" >&2
		exit 1
		;;
	esac

	case "$(uname -m)" in
	x86_64 | amd64) arch="amd64" ;;
	arm64 | aarch64) arch="arm64" ;;
	*)
		echo "error: unsupported architecture $(uname -m)" >&2
		exit 1
		;;
	esac

	asset="rad-$os-$arch.tar.gz"
	if [ -n "${RAD_VERSION:-}" ]; then
		base="https://github.com/$repo/releases/download/${RAD_VERSION}"
	else
		base="https://github.com/$repo/releases/latest/download"
	fi
	url="$base/$asset"

	echo "Downloading $url"
	mkdir -p "$bin_dir"
	tmp="$(mktemp -d)"
	trap 'rm -rf "$tmp"' EXIT

	if command -v curl >/dev/null 2>&1; then
		dl() { curl -fsSL "$1" -o "$2"; }
	elif command -v wget >/dev/null 2>&1; then
		dl() { wget -qO "$2" "$1"; }
	else
		echo "error: need curl or wget" >&2
		exit 1
	fi

	dl "$url" "$tmp/$asset"

	# Verify the download against the release's published checksums before
	# trusting its contents. SHA256SUMS is `sha256sum` output: "<hash>  <file>".
	dl "$base/SHA256SUMS" "$tmp/SHA256SUMS"
	expected="$(awk -v a="$asset" '$2 == a { print $1 }' "$tmp/SHA256SUMS")"
	if [ -z "$expected" ]; then
		echo "error: SHA256SUMS has no entry for $asset" >&2
		exit 1
	fi
	if command -v sha256sum >/dev/null 2>&1; then
		actual="$(sha256sum "$tmp/$asset" | awk '{ print $1 }')"
	elif command -v shasum >/dev/null 2>&1; then
		actual="$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')"
	else
		echo "error: need sha256sum or shasum to verify the download" >&2
		exit 1
	fi
	if [ "$expected" != "$actual" ]; then
		echo "error: checksum mismatch for $asset" >&2
		echo "  expected $expected" >&2
		echo "  actual   $actual" >&2
		exit 1
	fi

	tar -xzf "$tmp/$asset" -C "$tmp"
	if [ ! -f "$tmp/rad" ]; then
		echo "error: release archive does not contain rad" >&2
		exit 1
	fi
	chmod +x "$tmp/rad"

	# macOS: the binary is unsigned (no Apple developer certificate); clear the
	# quarantine flag so Gatekeeper doesn't block the validation run below.
	if [ "$os" = "darwin" ]; then
		xattr -d com.apple.quarantine "$tmp/rad" 2>/dev/null || true
	fi

	# Validate the downloaded binary before replacing a working installation.
	if ! "$tmp/rad" --version >/dev/null 2>&1; then
		echo "error: downloaded rad failed its version check" >&2
		exit 1
	fi

	mv "$tmp/rad" "$bin_dir/rad"

	echo ""
	echo "rad was installed to $bin_dir/rad"
	"$bin_dir/rad" --version

	case ":${PATH}:" in
	*":$bin_dir:"*) ;;
	*)
		shell_name="$(basename "${SHELL:-sh}")"
		case "$shell_name" in
		fish) profile="~/.config/fish/config.fish"; line="fish_add_path $bin_dir" ;;
		zsh) profile="~/.zshrc"; line="export PATH=\"$bin_dir:\$PATH\"" ;;
		bash) profile="~/.bashrc"; line="export PATH=\"$bin_dir:\$PATH\"" ;;
		nu) profile="~/.config/nushell/env.nu"; line="\$env.PATH = (\$env.PATH | prepend \"$bin_dir\")" ;;
		*) profile="your shell profile"; line="export PATH=\"$bin_dir:\$PATH\"" ;;
		esac
		echo ""
		echo "Add rad to your PATH by adding this to $profile:"
		echo ""
		echo "  $line"
		;;
	esac

	echo ""
	echo "Get started:"
	echo "  rad serve                 # start a database (RAD_STORAGE=memory|file|s3)"
	echo "  rad schema migrate        # uses rad.config.yaml and rad.schema.yaml"
	echo "  rad generate"
}

main "$@"
