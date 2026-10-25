#!/usr/bin/env bash
# Builds and installs Morlaco, or removes it again.
#
# On Arch Linux prefer the package in packaging/arch: pacman then knows about
# the files and can remove them. This script is for other systems and for
# installing into your home directory; pacman does not track what it installs.
set -euo pipefail

APP_ID="io.github.jeronimoalbi.Morlaco"

usage() {
    cat <<EOF
Usage: ./install.sh [--no-build] [--uninstall]

Installs the morlaco binary, its desktop file and its icons.

Options:
  --no-build    Install the existing target/release/morlaco without building.
  --uninstall   Remove the files this script installs.
  -h, --help    Show this help.

Environment:
  PREFIX    Where to install. Default: /usr/local as root, otherwise ~/.local
  DESTDIR   Staging directory prepended to every path, for packaging.
EOF
}

die() {
    echo "install.sh: $*" >&2
    exit 1
}

root=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
build=1
action=install
for arg in "$@"; do
    case "$arg" in
        --no-build) build=0 ;;
        --uninstall) action=uninstall ;;
        -h | --help) usage; exit 0 ;;
        *) echo "install.sh: unknown option: $arg" >&2; usage >&2; exit 2 ;;
    esac
done

if [[ -z "${PREFIX:-}" ]]; then
    if [[ $EUID -eq 0 ]]; then
        PREFIX=/usr/local
    else
        PREFIX="$HOME/.local"
    fi
fi
DESTDIR=${DESTDIR:-}

bin_file="$DESTDIR$PREFIX/bin/morlaco"
desktop_file="$DESTDIR$PREFIX/share/applications/$APP_ID.desktop"
icon_file="$DESTDIR$PREFIX/share/icons/hicolor/scalable/apps/$APP_ID.svg"
symbolic_file="$DESTDIR$PREFIX/share/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg"

# Refreshes the desktop and icon caches so the launcher shows the app at
# once. Only for real installs, and a missing tool is not an error.
refresh_caches() {
    [[ -z $DESTDIR ]] || return 0
    if command -v update-desktop-database >/dev/null; then
        update-desktop-database -q "$PREFIX/share/applications" || true
    fi
    if command -v gtk-update-icon-cache >/dev/null; then
        gtk-update-icon-cache -q -t -f "$PREFIX/share/icons/hicolor" || true
    fi
}

# Copies the desktop file, pointing Exec at the installed binary. A
# launcher cannot rely on the session PATH containing ~/.local/bin.
write_desktop_file() {
    local exec_path="$PREFIX/bin/morlaco" line
    if [[ $exec_path == *[[:space:]]* ]]; then
        exec_path="\"$exec_path\""
    fi
    mkdir -p "$(dirname "$desktop_file")"
    while IFS= read -r line; do
        if [[ $line == Exec=* ]]; then
            printf 'Exec=%s\n' "$exec_path"
        else
            printf '%s\n' "$line"
        fi
    done <"$root/data/$APP_ID.desktop" >"$desktop_file"
    chmod 644 "$desktop_file"
}

install_app() {
    if ((build)); then
        (cd "$root" && cargo build --release --locked -p morlaco)
    fi
    local binary="$root/target/release/morlaco"
    [[ -x $binary ]] || die "$binary not found; build first or drop --no-build"

    install -Dm755 "$binary" "$bin_file"
    write_desktop_file
    install -Dm644 "$root/app/data/icons/hicolor/scalable/apps/$APP_ID.svg" "$icon_file"
    install -Dm644 "$root/app/data/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg" "$symbolic_file"
    refresh_caches

    echo "Installed Morlaco under $DESTDIR$PREFIX"
    if [[ -z $DESTDIR && :$PATH: != *":$PREFIX/bin:"* ]]; then
        echo "Note: $PREFIX/bin is not in your PATH; start it from the launcher or with $bin_file"
    fi
}

uninstall_app() {
    local file removed=0
    for file in "$bin_file" "$desktop_file" "$icon_file" "$symbolic_file"; do
        if [[ -e $file ]]; then
            rm -f -- "$file"
            echo "Removed $file"
            removed=1
        fi
    done
    ((removed)) || echo "Nothing to remove under $DESTDIR$PREFIX"
    refresh_caches
}

"${action}_app"
