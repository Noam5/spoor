PREFIX ?= /usr/local
BIN    := target/release/spoor

.PHONY: build test install uninstall status logs install-krunner uninstall-krunner deb

build:
	cargo build --release

test:
	cargo test --release

# Deliberately does NOT depend on 'build': this target runs as root, and
# invoking cargo as root leaves root-owned artifacts in target/.
install:
	@test -x $(BIN) || { echo "run 'make build' as your user first"; exit 1; }
	install -Dm755 $(BIN) $(DESTDIR)$(PREFIX)/bin/spoor
	install -Dm644 packaging/spoor.service /etc/systemd/system/spoor.service
	install -Dm644 packaging/spoor.desktop \
	    $(DESTDIR)$(PREFIX)/share/applications/spoor.desktop
	install -Dm644 packaging/spoor.svg \
	    $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/spoor.svg
	@test -f $(DESTDIR)$(PREFIX)/share/icons/hicolor/index.theme || \
	    install -Dm644 packaging/hicolor-index.theme \
	        $(DESTDIR)$(PREFIX)/share/icons/hicolor/index.theme
	-gtk-update-icon-cache -qtf $(DESTDIR)$(PREFIX)/share/icons/hicolor 2>/dev/null
	install -Dm644 packaging/spoor.1 $(DESTDIR)$(PREFIX)/share/man/man1/spoor.1
	gzip -9nf $(DESTDIR)$(PREFIX)/share/man/man1/spoor.1
	sed 's|@BIN@|$(PREFIX)/bin/spoor|' packaging/org.spoor.configure.policy.in \
	    | install -Dm644 /dev/stdin /usr/share/polkit-1/actions/org.spoor.configure.policy
	systemctl daemon-reload
	-update-desktop-database $(DESTDIR)$(PREFIX)/share/applications 2>/dev/null
	@echo "installed. enable with: systemctl enable --now spoor"

uninstall:
	-systemctl disable --now spoor
	rm -f /etc/systemd/system/spoor.service $(DESTDIR)$(PREFIX)/bin/spoor
	rm -f $(DESTDIR)$(PREFIX)/share/applications/spoor.desktop $(DESTDIR)$(PREFIX)/share/applications/spoor-gui.desktop
	rm -f /usr/share/polkit-1/actions/org.spoor.configure.policy
	rm -f $(DESTDIR)$(PREFIX)/share/man/man1/spoor.1.gz
	rm -f $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/spoor.svg
	systemctl daemon-reload
	@echo "removed. left in place: /var/lib/spoor (snapshot), /etc/spoor (settings)"

status:
	systemctl status spoor --no-pager

logs:
	journalctl -u spoor -n 50 --no-pager

# KRunner integration is per-user and needs no root: the runner is an ordinary
# session process that talks to the daemon over its socket.
install-krunner: build
	mkdir -p $(HOME)/.local/share/krunner/dbusplugins $(HOME)/.local/share/dbus-1/services
	install -m644 packaging/spoor-krunner.desktop \
	    $(HOME)/.local/share/krunner/dbusplugins/spoor.desktop
	sed 's|@BIN@|$(PREFIX)/bin/spoor|' packaging/org.kde.spoor.service.in \
	    > $(HOME)/.local/share/dbus-1/services/org.kde.spoor.service
	@echo "installed. restart krunner to pick it up:  kquitapp6 krunner"
	@echo "note: needs $(PREFIX)/bin/spoor, i.e. 'sudo make install' first"

uninstall-krunner:
	rm -f $(HOME)/.local/share/krunner/dbusplugins/spoor.desktop
	rm -f $(HOME)/.local/share/dbus-1/services/org.kde.spoor.service
	@echo "removed. restart krunner:  kquitapp6 krunner"

# Debian/Ubuntu package in target/deb/ (installs to /usr, enables the service).
deb: build
	packaging/build-deb.sh
