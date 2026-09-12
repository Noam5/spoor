#!/bin/sh
# Build a Debian/Ubuntu package from target/release/spoor: `make deb`.
# Installs to /usr (not /usr/local), system-wide including the KRunner plugin,
# and enables the service on install.
set -eu
cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH=$(dpkg --print-architecture)
ROOT=target/deb/root
OUT=target/deb/spoor_${VERSION}_${ARCH}.deb
rm -rf target/deb && mkdir -p "$ROOT/DEBIAN"

install -Dm755 target/release/spoor "$ROOT/usr/bin/spoor"
sed 's|/usr/local/bin/spoor|/usr/bin/spoor|' packaging/spoor.service \
    | install -Dm644 /dev/stdin "$ROOT/usr/lib/systemd/system/spoor.service"
sed 's|/usr/local/bin/spoor|/usr/bin/spoor|' packaging/spoor-gui.desktop \
    | install -Dm644 /dev/stdin "$ROOT/usr/share/applications/spoor-gui.desktop"
install -Dm644 packaging/spoor-krunner.desktop "$ROOT/usr/share/krunner/dbusplugins/spoor.desktop"
sed 's|@BIN@|/usr/bin/spoor|' packaging/org.kde.spoor.service.in \
    | install -Dm644 /dev/stdin "$ROOT/usr/share/dbus-1/services/org.kde.spoor.service"
sed 's|@BIN@|/usr/bin/spoor|' packaging/org.spoor.configure.policy.in \
    | install -Dm644 /dev/stdin "$ROOT/usr/share/polkit-1/actions/org.spoor.configure.policy"
for f in README.md LICENSE-MIT LICENSE-APACHE; do install -Dm644 "$f" "$ROOT/usr/share/doc/spoor/$f"; done
install -Dm644 packaging/spoor.1 "$ROOT/usr/share/man/man1/spoor.1"
gzip -9nf "$ROOT/usr/share/man/man1/spoor.1"

# Runtime dependencies, computed from the binary rather than guessed.
mkdir -p target/deb/shlibs/debian
printf 'Source: spoor\n\nPackage: spoor\nArchitecture: any\n' > target/deb/shlibs/debian/control
DEPS=$(cd target/deb/shlibs && dpkg-shlibdeps -O "../../../$ROOT/usr/bin/spoor" 2>/dev/null | sed 's/^shlibs:Depends=//')

cat > "$ROOT/DEBIAN/control" <<EOF
Package: spoor
Version: $VERSION
Architecture: $ARCH
Maintainer: Noam <noam555@gmail.com>
Homepage: https://github.com/Noam5/spoor
Installed-Size: $(du -sk "$ROOT" | cut -f1)
Depends: $DEPS, systemd
Section: utils
Priority: optional
Description: instant file-name search for Linux
 A root daemon keeps a trigram index of the folders you choose (by default
 /home) current through fanotify, and
 serves it over a Unix socket to a CLI, a GTK window and a KDE KRunner plugin.
 Results are filtered by each caller's own permissions.
EOF
cat > "$ROOT/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = configure ] && [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    systemctl enable --now spoor.service || true
fi
EOF
cat > "$ROOT/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
if [ -d /run/systemd/system ]; then
    systemctl disable --now spoor.service || true
fi
EOF
cat > "$ROOT/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
[ -d /run/systemd/system ] && systemctl daemon-reload || true
if [ "$1" = purge ]; then rm -rf /var/lib/spoor /etc/spoor; fi
EOF
chmod 755 "$ROOT/DEBIAN/postinst" "$ROOT/DEBIAN/prerm" "$ROOT/DEBIAN/postrm"
dpkg-deb --build --root-owner-group "$ROOT" "$OUT" >/dev/null
echo "$OUT"
