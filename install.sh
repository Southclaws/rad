#!/bin/sh
# Rad installer for Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/Southclaws/rad/main/install.sh | sh
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
		url="https://github.com/$repo/releases/download/${RAD_VERSION}/$asset"
	else
		url="https://github.com/$repo/releases/latest/download/$asset"
	fi

	echo "Downloading $url"
	mkdir -p "$bin_dir"
	tmp="$(mktemp -d)"
	trap 'rm -rf "$tmp"' EXIT

	if command -v curl >/dev/null 2>&1; then
		curl -fsSL "$url" -o "$tmp/$asset"
	elif command -v wget >/dev/null 2>&1; then
		wget -qO "$tmp/$asset" "$url"
	else
		echo "error: need curl or wget" >&2
		exit 1
	fi

	tar -xzf "$tmp/$asset" -C "$tmp"
	mv "$tmp/rad" "$bin_dir/rad"
	chmod +x "$bin_dir/rad"

	# macOS: the binary is unsigned (no Apple developer certificate); clear
	# the quarantine flag if one was applied so Gatekeeper doesn't block it.
	if [ "$os" = "darwin" ]; then
		xattr -d com.apple.quarantine "$bin_dir/rad" 2>/dev/null || true
	fi

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
	echo "  rad migrate -u rad://localhost -f schema.rad"
	echo "  rad generate -f schema.rad -o ./generated --pkg db"
}

main "$@"
