#!/bin/sh
# Install ridgewarden-planner on a Linux box that already runs the ridgewarden core.
#
#   curl -fsSL https://github.com/Londopy/ridgewarden-planner/releases/latest/download/install.sh | sudo sh
#
# Optional: RIDGEWARDEN_PLANNER_VERSION=0.2.0 to pin a version.
set -eu

REPO=Londopy/ridgewarden-planner
BIN=/usr/local/bin/ridgewarden-planner

if [ "$(id -u)" -ne 0 ]; then
    echo "Run as root (with sudo)." >&2
    exit 1
fi

# the core checks this release's signature, and the app needs the core to run
core=$(command -v ridgewarden || true)
if [ -z "$core" ]; then
    echo "Install the ridgewarden core first: https://github.com/Londopy/ridgewarden" >&2
    exit 1
fi

case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *)
        echo "No build for $(uname -m). 64-bit Raspberry Pi OS and x86_64 Linux are supported." >&2
        exit 1
        ;;
esac

pinned=${RIDGEWARDEN_PLANNER_VERSION:-}
if [ -n "$pinned" ]; then
    tag="v$pinned"
else
    tag=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
        sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
fi
[ -n "$tag" ] || { echo "Could not find the latest release." >&2; exit 1; }
ver=${tag#v}
file="ridgewarden-planner-v$ver-$arch-linux.tar.gz"
base="https://github.com/$REPO/releases/download/$tag"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "Downloading ridgewarden-planner $tag for $arch"
for f in "$file" SHA256SUMS.txt SHA256SUMS.txt.sig; do
    curl -fsSL -o "$tmp/$f" "$base/$f"
done

# the installed core checks the signature against the release key it already trusts
(cd "$tmp" && grep " $file\$" SHA256SUMS.txt | sha256sum -c -)
"$core" verify-release "$tmp/SHA256SUMS.txt"
tar -xzf "$tmp/$file" -C "$tmp"
dir="$tmp/ridgewarden-planner-v$ver-$arch-linux"

install -m 755 "$dir/ridgewarden-planner" "$BIN"
if ! id ridgewarden-planner >/dev/null 2>&1; then
    useradd --system --home-dir /var/lib/ridgewarden-planner --shell /usr/sbin/nologin --user-group ridgewarden-planner
fi
usermod -a -G ridgewarden ridgewarden-planner
install -d -m 750 -o ridgewarden-planner -g ridgewarden-planner /var/lib/ridgewarden-planner /var/lib/ridgewarden-planner/terrain
# an upgrade from when apps ran as the core's user leaves files it owned
chown -R ridgewarden-planner:ridgewarden-planner /var/lib/ridgewarden-planner
# terrain tiles pass through the core's import and export folders
install -d -m 2770 -o ridgewarden -g ridgewarden /var/lib/ridgewarden/imports /var/lib/ridgewarden/exports
if [ ! -f /etc/ridgewarden/planner.toml ]; then
    "$BIN" --config /etc/ridgewarden/planner.toml init
fi
# its webhook URLs and passphrases are for this app only, not the other apps
# in the ridgewarden group
chown root:ridgewarden-planner /etc/ridgewarden/planner.toml
chmod 640 /etc/ridgewarden/planner.toml

if [ -d /run/systemd/system ]; then
    sed "s#/usr/bin/ridgewarden-planner#$BIN#" "$dir/ridgewarden-planner.service" >/etc/systemd/system/ridgewarden-planner.service
    systemctl daemon-reload
    systemctl enable ridgewarden-planner
    if [ -f /etc/ridgewarden/ridgewarden.toml ]; then
        # restart, so an upgrade replaces a running old version
        systemctl restart ridgewarden-planner
    else
        echo "Set up the core first (ridgewarden init), then: sudo systemctl start ridgewarden-planner"
        exit 0
    fi
    echo "ridgewarden-planner is running: http://$(hostname):8794/"
else
    echo "Installed $BIN ($tag). Start it with: $BIN --config /etc/ridgewarden/planner.toml run"
fi
