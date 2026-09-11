//! Standalone GTK3 search window, laid out like FSearch: a menu bar with the
//! same Search options and shortcuts (Ctrl+I match case, Ctrl+R regex, Ctrl+U
//! search in path), a right-click menu on results, a Preferences dialog, and
//! settings persisted in ~/.config/spoor/gui.conf.
//!
//! A thin client: it holds no index of its own and queries the daemon over
//! /run/spoor.sock. Closing it cannot invalidate anything -- which is the whole
//! point of keeping the index in a daemon.

use crate::index::{Kind, SearchOpts};
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::{
    AccelFlags, AccelGroup, AppChooserDialog, Application, ApplicationWindow, Box as GtkBox,
    Button, CellRendererText, CheckButton, CheckMenuItem, Dialog, DialogFlags, Entry, Label,
    ListStore, Menu, MenuBar, MenuItem, Orientation, PolicyType, RadioMenuItem, ResponseType,
    ScrolledWindow, SeparatorMenuItem, SpinButton, TreeView, TreeViewColumn,
};
use std::cell::RefCell;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

const COL_NAME: u32 = 0;
const COL_PATH: u32 = 1;
const COL_SIZE: u32 = 2;
const COL_MODIFIED: u32 = 3;
const COL_FULL: u32 = 4;

/// Everything the user can set, persisted between runs.
#[derive(Clone)]
struct Settings {
    opts: SearchOpts,
    search_as_you_type: bool,
    max_results: u32,
}

impl Settings {
    fn file() -> PathBuf {
        glib::user_config_dir().join("spoor").join("gui.conf")
    }

    fn load() -> Settings {
        let mut s = Settings {
            opts: SearchOpts::default(),
            search_as_you_type: true,
            max_results: 500,
        };
        let kf = glib::KeyFile::new();
        if kf.load_from_file(Self::file(), glib::KeyFileFlags::NONE).is_err() {
            return s;
        }
        if let Ok(v) = kf.boolean("search", "match_case") {
            s.opts.match_case = v;
        }
        if let Ok(v) = kf.boolean("search", "regex") {
            s.opts.regex = v;
        }
        if let Ok(v) = kf.boolean("search", "search_in_path") {
            s.opts.in_path = v;
        }
        if let Ok(v) = kf.string("search", "filter") {
            s.opts.kind = match v.as_str() {
                "files" => Kind::Files,
                "folders" => Kind::Folders,
                _ => Kind::All,
            };
        }
        if let Ok(v) = kf.boolean("search", "show_hidden") {
            s.opts.hide_hidden = !v;
        }
        if let Ok(v) = kf.boolean("search", "search_as_you_type") {
            s.search_as_you_type = v;
        }
        if let Ok(v) = kf.integer("search", "max_results") {
            s.max_results = v.clamp(100, 20_000) as u32;
        }
        s
    }

    fn save(&self) {
        let kf = glib::KeyFile::new();
        kf.set_boolean("search", "match_case", self.opts.match_case);
        kf.set_boolean("search", "regex", self.opts.regex);
        kf.set_boolean("search", "search_in_path", self.opts.in_path);
        kf.set_string(
            "search",
            "filter",
            match self.opts.kind {
                Kind::All => "all",
                Kind::Files => "files",
                Kind::Folders => "folders",
            },
        );
        kf.set_boolean("search", "show_hidden", !self.opts.hide_hidden);
        kf.set_boolean("search", "search_as_you_type", self.search_as_you_type);
        kf.set_integer("search", "max_results", self.max_results as i32);
        let path = Self::file();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, kf.to_data().as_str()) {
            eprintln!("spoor: cannot save {}: {}", path.display(), e);
        }
    }
}

struct Ui {
    window: ApplicationWindow,
    entry: Entry,
    store: ListStore,
    tree: TreeView,
    status: Label,
    settings: RefCell<Settings>,
    /// /run/spoor.sock, unless --socket was given (used for testing).
    socket: String,
    /// Debounce timer for search-as-you-type.
    pending: RefCell<Option<glib::SourceId>>,
}

pub fn run(socket_override: Option<String>, default_socket: &str) {
    let socket = socket_override.unwrap_or_else(|| default_socket.to_string());
    let app = Application::builder()
        .application_id("org.spoor.gui")
        .build();
    app.connect_activate(move |app| build(app, socket.clone()));
    // Our own argv is already parsed; do not let GTK see it.
    app.run_with_args::<&str>(&[]);
}

fn build(app: &Application, socket: String) {
    // Single instance: launching again brings the existing window forward.
    if let Some(w) = app.active_window() {
        w.present();
        return;
    }

    let window = ApplicationWindow::builder()
        .application(app)
        .title("spoor")
        .default_width(1000)
        .default_height(640)
        .build();
    let accel = AccelGroup::new();
    window.add_accel_group(&accel);

    let root = GtkBox::new(Orientation::Vertical, 0);
    let menubar = MenuBar::new();
    root.pack_start(&menubar, false, false, 0);

    let bar = GtkBox::new(Orientation::Horizontal, 6);
    bar.set_margin_top(8);
    bar.set_margin_bottom(8);
    bar.set_margin_start(8);
    bar.set_margin_end(8);
    let entry = Entry::builder()
        .placeholder_text("Search files…")
        .hexpand(true)
        .build();
    entry.set_icon_from_icon_name(gtk::EntryIconPosition::Primary, Some("system-search"));
    let open_folder = Button::with_label("Open Folder");
    bar.pack_start(&entry, true, true, 0);
    bar.pack_start(&open_folder, false, false, 0);
    root.pack_start(&bar, false, false, 0);

    let store = ListStore::new(&[String::static_type(); 5]);
    let tree = TreeView::with_model(&store);
    tree.set_headers_visible(true);
    tree.set_fixed_height_mode(true);
    for (title, col, width) in [
        ("Name", COL_NAME, 320),
        ("Path", COL_PATH, 420),
        ("Size", COL_SIZE, 90),
        ("Modified", COL_MODIFIED, 150),
    ] {
        let r = CellRendererText::new();
        r.set_property("ellipsize", gtk::pango::EllipsizeMode::End);
        let c = TreeViewColumn::new();
        c.set_title(title);
        CellLayoutExt::pack_start(&c, &r, true);
        CellLayoutExt::add_attribute(&c, &r, "text", col as i32);
        c.set_resizable(true);
        c.set_sizing(gtk::TreeViewColumnSizing::Fixed);
        c.set_fixed_width(width);
        tree.append_column(&c);
    }
    let scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(PolicyType::Automatic)
        .vscrollbar_policy(PolicyType::Automatic)
        .build();
    scroll.add(&tree);
    root.pack_start(&scroll, true, true, 0);

    let status = Label::new(None);
    status.set_xalign(0.0);
    status.set_margin_top(4);
    status.set_margin_bottom(6);
    status.set_margin_start(10);
    root.pack_start(&status, false, false, 0);
    window.add(&root);

    let ui = Rc::new(Ui {
        window: window.clone(),
        entry: entry.clone(),
        store,
        tree: tree.clone(),
        status,
        settings: RefCell::new(Settings::load()),
        socket,
        pending: RefCell::new(None),
    });

    build_menus(&ui, &menubar, &accel);
    let context = build_context_menu(&ui);

    {
        let ui = ui.clone();
        entry.connect_changed(move |_| {
            if ui.settings.borrow().search_as_you_type {
                schedule_search(&ui);
            }
        });
    }
    {
        let ui = ui.clone();
        entry.connect_activate(move |_| {
            cancel_pending(&ui);
            run_search(&ui);
        });
    }
    // Double-click or Enter anywhere on a row opens the file, whichever column.
    tree.connect_row_activated(move |tv, path, _| {
        let full = tv
            .model()
            .and_then(|m| m.iter(path).map(|it| m.value(&it, COL_FULL as i32)))
            .and_then(|v| v.get::<String>().ok());
        if let Some(full) = full {
            open(&full);
        }
    });
    {
        let menu = context.clone();
        tree.connect_button_press_event(move |tv, ev| {
            if ev.event_type() != gtk::gdk::EventType::ButtonPress || ev.button() != 3 {
                return glib::Propagation::Proceed;
            }
            // Rows only: the column headers are a separate window.
            if ev.window() != tv.bin_window() {
                return glib::Propagation::Proceed;
            }
            let (x, y) = ev.position();
            if let Some((Some(path), _, _, _)) = tv.path_at_pos(x as i32, y as i32) {
                // Right-click selects the row under the pointer first, so the
                // menu always acts on what was clicked.
                tv.selection().select_path(&path);
                tv.grab_focus();
                menu.popup_at_pointer(None);
            }
            glib::Propagation::Stop
        });
    }
    {
        // Menu key / Shift+F10 on a selected row.
        let menu = context.clone();
        let ui = ui.clone();
        tree.connect_popup_menu(move |_| {
            if selected_path(&ui).is_none() {
                return false;
            }
            menu.popup_at_pointer(None);
            true
        });
    }
    {
        let ui = ui.clone();
        open_folder.connect_clicked(move |_| {
            if let Some(p) = selected_path(&ui) {
                open(parent_dir(&p));
            }
        });
    }

    show_connection(&ui);
    window.show_all();
    entry.grab_focus();
}

// ---------------------------------------------------------------- menus

fn add_accel(w: &impl IsA<gtk::Widget>, group: &AccelGroup, spec: &str) {
    let (key, mods) = gtk::accelerator_parse(spec);
    w.add_accelerator("activate", group, key, mods, AccelFlags::VISIBLE);
}

fn submenu(bar: &MenuBar, label: &str) -> Menu {
    let menu = Menu::new();
    let top = MenuItem::with_mnemonic(label);
    top.set_submenu(Some(&menu));
    bar.append(&top);
    menu
}

fn action(menu: &Menu, label: &str, accel: Option<(&AccelGroup, &str)>, f: impl Fn() + 'static) {
    let mi = MenuItem::with_mnemonic(label);
    if let Some((group, spec)) = accel {
        add_accel(&mi, group, spec);
    }
    mi.connect_activate(move |_| f());
    menu.append(&mi);
}

fn toggle(
    menu: &Menu,
    label: &str,
    accel: Option<(&AccelGroup, &str)>,
    active: bool,
    f: impl Fn(bool) + 'static,
) {
    let mi = CheckMenuItem::with_mnemonic(label);
    mi.set_active(active); // before connecting, so loading settings is silent
    if let Some((group, spec)) = accel {
        add_accel(&mi, group, spec);
    }
    mi.connect_toggled(move |m| f(m.is_active()));
    menu.append(&mi);
}

fn build_menus(ui: &Rc<Ui>, bar: &MenuBar, accel: &AccelGroup) {
    let s = ui.settings.borrow().clone();

    let file = submenu(bar, "_File");
    {
        let ui = ui.clone();
        action(&file, "_Open", None, move || {
            if let Some(p) = selected_path(&ui) {
                open(&p);
            }
        });
    }
    {
        let ui = ui.clone();
        action(&file, "Open _With…", None, move || open_with(&ui));
    }
    {
        let ui = ui.clone();
        action(&file, "Open Containing _Folder", Some((accel, "<Control>Return")), move || {
            if let Some(p) = selected_path(&ui) {
                open(parent_dir(&p));
            }
        });
    }
    file.append(&SeparatorMenuItem::new());
    {
        let ui = ui.clone();
        action(&file, "P_roperties…", Some((accel, "<Alt>Return")), move || show_properties(&ui));
    }
    file.append(&SeparatorMenuItem::new());
    {
        let win = ui.window.clone();
        action(&file, "_Quit", Some((accel, "<Control>q")), move || win.close());
    }

    let edit = submenu(bar, "_Edit");
    {
        let ui = ui.clone();
        action(&edit, "Copy _Path", Some((accel, "<Control><Shift>c")), move || {
            copy_selected(&ui, false)
        });
    }
    {
        let ui = ui.clone();
        action(&edit, "Copy _Name", None, move || copy_selected(&ui, true));
    }
    edit.append(&SeparatorMenuItem::new());
    {
        let ui = ui.clone();
        action(&edit, "_Preferences", Some((accel, "<Control>p")), move || {
            show_preferences(&ui)
        });
    }

    // Same labels and shortcuts as FSearch. Everything calls "Search in Path"
    // "Match Path"; it is one option, not two.
    let search = submenu(bar, "_Search");
    {
        let ui = ui.clone();
        toggle(&search, "Match _Case", Some((accel, "<Control>i")), s.opts.match_case, move |on| {
            set_opt(&ui, |o| o.match_case = on)
        });
    }
    {
        let ui = ui.clone();
        toggle(&search, "Enable _Regex", Some((accel, "<Control>r")), s.opts.regex, move |on| {
            set_opt(&ui, |o| o.regex = on)
        });
    }
    {
        let ui = ui.clone();
        toggle(&search, "Search in _Path", Some((accel, "<Control>u")), s.opts.in_path, move |on| {
            set_opt(&ui, |o| o.in_path = on)
        });
    }
    search.append(&SeparatorMenuItem::new());
    let all = RadioMenuItem::builder()
        .label("_All Results")
        .use_underline(true)
        .build();
    let files = RadioMenuItem::with_mnemonic_from_widget(&all, Some("_Files Only"));
    let folders = RadioMenuItem::with_mnemonic_from_widget(&all, Some("F_olders Only"));
    let radios = [(all, Kind::All), (files, Kind::Files), (folders, Kind::Folders)];
    for (item, kind) in &radios {
        search.append(item);
        if s.opts.kind == *kind {
            item.set_active(true);
        }
    }
    for (item, kind) in radios {
        let ui = ui.clone();
        item.connect_toggled(move |m| {
            if m.is_active() {
                set_opt(&ui, |o| o.kind = kind);
            }
        });
    }

    let help = submenu(bar, "_Help");
    {
        let ui = ui.clone();
        action(&help, "_Search Syntax", Some((accel, "F1")), move || show_syntax(&ui));
    }
    {
        let ui = ui.clone();
        action(&help, "_About", None, move || show_about(&ui));
    }
}

/// Right-click menu on a result row. No keyboard accelerators here: window
/// accelerators fire before the search box sees the key, so binding Delete to
/// Move to Trash would trash the selected file while you type.
fn build_context_menu(ui: &Rc<Ui>) -> Menu {
    let menu = Menu::new();
    {
        let ui = ui.clone();
        action(&menu, "_Open", None, move || {
            if let Some(p) = selected_path(&ui) {
                open(&p);
            }
        });
    }
    {
        let ui = ui.clone();
        action(&menu, "Open _With…", None, move || open_with(&ui));
    }
    {
        let ui = ui.clone();
        action(&menu, "Open Containing _Folder", None, move || {
            if let Some(p) = selected_path(&ui) {
                open(parent_dir(&p));
            }
        });
    }
    menu.append(&SeparatorMenuItem::new());
    {
        let ui = ui.clone();
        action(&menu, "Copy _Path", None, move || copy_selected(&ui, false));
    }
    {
        let ui = ui.clone();
        action(&menu, "Copy _Name", None, move || copy_selected(&ui, true));
    }
    menu.append(&SeparatorMenuItem::new());
    {
        let ui = ui.clone();
        action(&menu, "Move to _Trash", None, move || trash_selected(&ui));
    }
    menu.append(&SeparatorMenuItem::new());
    {
        let ui = ui.clone();
        action(&menu, "P_roperties…", None, move || show_properties(&ui));
    }
    menu.set_attach_widget(Some(&ui.tree));
    menu.show_all();
    menu
}

fn set_opt(ui: &Rc<Ui>, f: impl FnOnce(&mut SearchOpts)) {
    {
        let mut s = ui.settings.borrow_mut();
        f(&mut s.opts);
        s.save();
    }
    run_search(ui);
}

// ---------------------------------------------------------------- search

/// Search runs on the GTK main thread. Plain queries answer in well under a
/// millisecond, but a regex is a full scan (~150ms on names, ~450ms on paths at
/// 2.3M files), so searching on every keystroke would freeze the window once
/// per letter. Wait for a pause in typing instead, longer when regex is on.
fn schedule_search(ui: &Rc<Ui>) {
    cancel_pending(ui);
    let delay = if ui.settings.borrow().opts.regex { 300 } else { 40 };
    let ui2 = ui.clone();
    let id = glib::timeout_add_local_once(std::time::Duration::from_millis(delay), move || {
        // Forget the id before running: removing a source that already fired
        // is an error in GLib.
        ui2.pending.borrow_mut().take();
        run_search(&ui2);
    });
    *ui.pending.borrow_mut() = Some(id);
}

fn cancel_pending(ui: &Ui) {
    if let Some(id) = ui.pending.borrow_mut().take() {
        id.remove();
    }
}

fn run_search(ui: &Ui) {
    let text = ui.entry.text().to_string();
    let pattern = text.trim();
    let s = ui.settings.borrow().clone();
    let badges = badges(&s.opts);
    ui.store.clear();
    if pattern.is_empty() {
        ui.status.set_text(&format!("type to search{}", badges));
        return;
    }

    let t0 = Instant::now();
    let result = crate::query::search(&ui.socket, pattern, s.max_results as usize, &s.opts);
    let elapsed = t0.elapsed();
    let hits = match result {
        Ok(h) => h,
        Err(e) => {
            ui.status.set_text(&format!("{}{}", e, badges));
            return;
        }
    };

    for path in &hits {
        let name = path.rsplit('/').next().unwrap_or(path);
        let (size, modified) = match std::fs::symlink_metadata(path) {
            Ok(m) if m.is_dir() => ("—".to_string(), fmt_time(m.mtime())),
            Ok(m) => (fmt_size(m.size()), fmt_time(m.mtime())),
            Err(_) => ("?".to_string(), String::new()),
        };
        ui.store.insert_with_values(
            None,
            &[
                (COL_NAME, &name),
                (COL_PATH, &parent_dir(path)),
                (COL_SIZE, &size.as_str()),
                (COL_MODIFIED, &modified.as_str()),
                (COL_FULL, &path.as_str()),
            ],
        );
    }
    let capped = if hits.len() >= s.max_results as usize {
        " (limit reached)"
    } else {
        ""
    };
    ui.status.set_text(&format!(
        "{} result{}{} in {:.1} ms{}",
        hits.len(),
        if hits.len() == 1 { "" } else { "s" },
        capped,
        elapsed.as_secs_f64() * 1000.0,
        badges
    ));
}

/// Active options, shown in the status line the way FSearch flags them.
fn badges(o: &SearchOpts) -> String {
    let mut v = Vec::new();
    if o.match_case {
        v.push("CASE");
    }
    if o.regex {
        v.push("REGEX");
    }
    if o.in_path {
        v.push("PATH");
    }
    match o.kind {
        Kind::Files => v.push("FILES"),
        Kind::Folders => v.push("FOLDERS"),
        Kind::All => {}
    }
    if o.hide_hidden {
        v.push("NO HIDDEN");
    }
    if v.is_empty() {
        String::new()
    } else {
        format!("     [{}]", v.join("] ["))
    }
}

fn show_connection(ui: &Ui) {
    let badges = badges(&ui.settings.borrow().opts);
    let text = match crate::query::stats(&ui.socket) {
        Some(st) => {
            let n = st
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
                .map(group_digits)
                .unwrap_or(st);
            format!("{} entries indexed", n)
        }
        None => format!(
            "NOT connected: no daemon answering at {} — is spoor.service running?",
            ui.socket
        ),
    };
    ui.status.set_text(&format!("{}{}", text, badges));
}

fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------- actions

fn selected_path(ui: &Ui) -> Option<String> {
    let (model, iter) = ui.tree.selection().selected()?;
    model.value(&iter, COL_FULL as i32).get::<String>().ok()
}

fn copy_selected(ui: &Ui, name_only: bool) {
    let Some(p) = selected_path(ui) else { return };
    let text = if name_only {
        p.rsplit('/').next().unwrap_or(&p).to_string()
    } else {
        p
    };
    gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD).set_text(&text);
    ui.status.set_text(&format!("copied {}", text));
}

fn open_with(ui: &Ui) {
    let Some(p) = selected_path(ui) else { return };
    let file = gio::File::for_path(&p);
    let dialog = AppChooserDialog::new(
        Some(&ui.window),
        DialogFlags::MODAL | DialogFlags::DESTROY_WITH_PARENT,
        &file,
    );
    dialog.connect_response(move |d, resp| {
        if resp == ResponseType::Ok {
            if let Some(app) = d.app_info() {
                let _ = app.launch(&[file.clone()], None::<&gio::AppLaunchContext>);
            }
        }
        d.close();
    });
    dialog.show_all();
}

/// Opens the file manager's own Properties dialog (Dolphin on KDE, Nautilus
/// on GNOME) through the freedesktop FileManager1 interface, the same one
/// FSearch uses. Dolphin only replies once that dialog is closed, so the call
/// is asynchronous with no timeout; only a real failure, such as no file
/// manager providing the interface, is reported.
fn show_properties(ui: &Rc<Ui>) {
    let Some(p) = selected_path(ui) else { return };
    // gio percent-encodes the URI, so names with spaces or '#' survive.
    let uri = gio::File::for_path(&p).uri().to_string();
    let conn = match gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>) {
        Ok(c) => c,
        Err(e) => {
            ui.status.set_text(&format!("could not open Properties: {}", e.message()));
            return;
        }
    };
    let ui2 = ui.clone();
    conn.call(
        Some("org.freedesktop.FileManager1"),
        "/org/freedesktop/FileManager1",
        "org.freedesktop.FileManager1",
        "ShowItemProperties",
        Some(&(vec![uri], "").to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        i32::MAX, // G_MAXINT: no timeout
        None::<&gio::Cancellable>,
        move |res| {
            if let Err(e) = res {
                ui2.status.set_text(&format!("could not open Properties: {}", e.message()));
            }
        },
    );
}

/// Moves the selected file to the desktop trash (recoverable), and drops the
/// row. The daemon sees the move through fanotify on its own.
fn trash_selected(ui: &Ui) {
    let Some((_, iter)) = ui.tree.selection().selected() else { return };
    let Ok(path) = ui.store.value(&iter, COL_FULL as i32).get::<String>() else {
        return;
    };
    match gio::File::for_path(&path).trash(None::<&gio::Cancellable>) {
        Ok(()) => {
            ui.store.remove(&iter);
            ui.status.set_text(&format!("moved to trash: {}", path));
        }
        Err(e) => ui.status.set_text(&format!("could not move to trash: {}", e.message())),
    }
}

fn open(target: &str) {
    let t = target.to_string();
    // Reap the child, so opening files repeatedly does not accumulate zombies.
    std::thread::spawn(move || {
        if let Ok(mut c) = std::process::Command::new("xdg-open").arg(&t).spawn() {
            let _ = c.wait();
        }
    });
}

fn parent_dir(p: &str) -> &str {
    match p.rfind('/') {
        Some(0) => "/",
        Some(i) => &p[..i],
        None => "/",
    }
}

// ---------------------------------------------------------------- dialogs

fn show_preferences(ui: &Rc<Ui>) {
    let dialog = Dialog::with_buttons(
        Some("Preferences"),
        Some(&ui.window),
        DialogFlags::MODAL | DialogFlags::DESTROY_WITH_PARENT,
        &[("_Close", ResponseType::Close)],
    );
    dialog.set_default_width(460);
    let s = ui.settings.borrow().clone();

    let content = GtkBox::new(Orientation::Vertical, 8);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let sayt = CheckButton::with_mnemonic("Search as you _type (otherwise press Enter)");
    sayt.set_active(s.search_as_you_type);
    let hidden = CheckButton::with_mnemonic("Show _hidden files and folders");
    hidden.set_active(!s.opts.hide_hidden);
    let limit_row = GtkBox::new(Orientation::Horizontal, 8);
    let limit_label = Label::with_mnemonic("_Maximum results:");
    let limit = SpinButton::with_range(100.0, 20_000.0, 100.0);
    limit.set_value(s.max_results as f64);
    limit_label.set_mnemonic_widget(Some(&limit));
    limit_row.pack_start(&limit_label, false, false, 0);
    limit_row.pack_start(&limit, false, false, 0);
    let note = Label::new(Some(
        "Match Case, Enable Regex, Search in Path and the Files / Folders filter \
         are on the Search menu (Ctrl+I, Ctrl+R, Ctrl+U).",
    ));
    note.set_xalign(0.0);
    note.set_line_wrap(true);
    note.style_context().add_class("dim-label");

    content.pack_start(&sayt, false, false, 0);
    content.pack_start(&hidden, false, false, 0);
    content.pack_start(&limit_row, false, false, 0);
    content.pack_start(&note, false, false, 8);

    {
        let ui = ui.clone();
        sayt.connect_toggled(move |b| {
            let mut s = ui.settings.borrow_mut();
            s.search_as_you_type = b.is_active();
            s.save();
        });
    }
    {
        let ui = ui.clone();
        hidden.connect_toggled(move |b| {
            let show = b.is_active();
            set_opt(&ui, |o| o.hide_hidden = !show)
        });
    }
    {
        let ui = ui.clone();
        limit.connect_value_changed(move |sb| {
            {
                let mut s = ui.settings.borrow_mut();
                s.max_results = sb.value_as_int().clamp(100, 20_000) as u32;
                s.save();
            }
            run_search(&ui);
        });
    }

    dialog.content_area().pack_start(&content, true, true, 0);
    dialog.connect_response(|d, _| d.close());
    dialog.show_all();
}

fn show_syntax(ui: &Ui) {
    let text = "<tt>\
plain text       part of a file name, anywhere in it
                 invoice   →  invoice-2024-03.pdf

contains  /      matched against the full path instead
                 projects/invoice

several words    all must appear, each anywhere in the path
                 wedding jpg  →  photos/wedding 2019/IMG-1.jpg
\"a phrase\"       quotes keep words together, spaces included

Search in Path   Ctrl+U   full-path matching without a /
Match Case       Ctrl+I   otherwise case is ignored
Enable Regex     Ctrl+R   the query is a regular expression
                          ^invoice-.*\\.pdf$

Files / Folders  Search menu   restrict results by type
Hidden files     shown by default; Preferences to hide
</tt>";
    let d = gtk::MessageDialog::new(
        Some(&ui.window),
        DialogFlags::MODAL | DialogFlags::DESTROY_WITH_PARENT,
        gtk::MessageType::Info,
        gtk::ButtonsType::Close,
        "Search syntax",
    );
    d.set_secondary_use_markup(true);
    d.set_secondary_text(Some(text));
    d.connect_response(|d, _| d.close());
    d.show_all();
}

fn show_about(ui: &Ui) {
    let d = gtk::AboutDialog::builder()
        .transient_for(&ui.window)
        .modal(true)
        .program_name("spoor")
        .version(env!("CARGO_PKG_VERSION"))
        .comments(
            "Instant file search for Linux.\n\
             A root daemon keeps the index live through fanotify;\n\
             this window only asks it questions.",
        )
        .logo_icon_name("system-search")
        .build();
    d.connect_response(|d, _| d.close());
    d.show();
}

// ---------------------------------------------------------------- formatting

fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

/// Local time via GLib. An earlier version spawned `date` once per row, i.e.
/// up to 500 processes per keystroke.
fn fmt_time(epoch: i64) -> String {
    glib::DateTime::from_unix_local(epoch)
        .and_then(|d| d.format("%Y-%m-%d %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}
