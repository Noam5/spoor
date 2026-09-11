PREFIX ?= /usr/local
BIN    := target/release/spoor

.PHONY: build test install uninstall status logs install-krunner uninstall-krunner

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
	install -Dm644 packaging/spoor-gui.desktop \
	    $(DESTDIR)$(PREFIX)/share/applications/spoor-gui.desktop
	systemctl daemon-reload
	-update-desktop-database $(DESTDIR)$(PREFIX)/share/applications 2>/dev/null
	@echo "installed. enable with: systemctl enable --now spoor"

uninstall:
	-systemctl disable --now spoor
	rm -f /etc/systemd/system/spoor.service $(DESTDIR)$(PREFIX)/bin/spoor
	rm -f $(DESTDIR)$(PREFIX)/share/applications/spoor-gui.desktop
	systemctl daemon-reload
	@echo "removed. snapshot left at /var/lib/spoor (delete manually if unwanted)"

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
