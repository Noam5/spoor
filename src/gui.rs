//! Standalone GTK3 search window, laid out like FSearch: a menu bar with the
//! same Search options and shortcuts (Ctrl+I match case, Ctrl+R regex, Ctrl+U
//! search in path), a right-click menu on results, a Preferences dialog, and
//! settings persisted in ~/.config/spoor/gui.conf.
//!
//! A thin client: it holds no index of its own and queries the daemon over
//! /run/spoor.sock. Closing it cannot invalidate anything -- which is the whole
//! point of keeping the index in a daemon.
//!
//! Paths stay raw bytes from the socket to xdg-open: a Linux file name need not
//! be UTF-8, and a GTK string column would mangle it, so the store holds only
//! display text plus an index into `Ui::paths`.

use crate::config;
use crate::index::{Kind, SearchOpts};
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::{
    AccelFlags, AccelGroup, AppChooserDialog, Application, ApplicationWindow, Box as GtkBox,
    Button, ButtonsType, CellRendererText, CheckButton, CheckMenuItem, Dialog, DialogFlags, Entry,
    FileChooserAction, FileChooserDialog, Label, ListStore, Menu, MenuBar, MenuItem, MessageDialog,
    MessageType, Orientation, PolicyType, RadioMenuItem, ResponseType, ScrolledWindow,
    SelectionMode, SeparatorMenuItem, SpinButton, TargetEntry, TargetFlags, TreeRowReference,
    TreeView, TreeViewColumn,
};
use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

const COL_NAME: u32 = 0;
const COL_PATH: u32 = 1;
const COL_SIZE: u32 = 2;
const COL_MODIFIED: u32 = 3;
/// The row's index into `Ui::paths`.
const COL_ID: u32 = 4;
/// Numeric sort keys behind Size and Modified; -1 for folders and for files
/// that vanished, so those sort together at one end.
const COL_SIZE_N: u32 = 5;
const COL_MTIME_N: u32 = 6;

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
        if kf
            .load_from_file(Self::file(), glib::KeyFileFlags::NONE)
            .is_err()
        {
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
    /// Raw path bytes of the rows on display, indexed by COL_ID.
    paths: RefCell<Vec<Vec<u8>>>,
    /// Number of the latest search started, and of the latest one shown. A
    /// worker's answer is dropped if a newer search has started since.
    started: Cell<u64>,
    shown: Cell<u64>,
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
    // Names the window's icon, and (on X11) the taskbar entry; on Wayland the
    // shell matches the desktop entry through StartupWMClass instead. It calls
    // into GTK, so it belongs here, after activation, not while the
    // Application is being built -- doing it earlier panics.
    gtk::Window::set_default_icon_name("spoor");
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

    let store = ListStore::new(&[
        String::static_type(),
        String::static_type(),
        String::static_type(),
        String::static_type(),
        u32::static_type(),
        i64::static_type(),
        i64::static_type(),
    ]);
    let tree = TreeView::with_model(&store);
    // Shift and Ctrl extend the selection, as in a file manager. Every action
    // below works on the whole selection, not just the row that was clicked.
    tree.selection().set_mode(SelectionMode::Multiple);
    tree.set_headers_visible(true);
    tree.set_fixed_height_mode(true);
    // Click a header to sort; results arrive in index order until then.
    for (title, col, sort, width) in [
        ("Name", COL_NAME, COL_NAME, 320),
        ("Path", COL_PATH, COL_PATH, 420),
        ("Size", COL_SIZE, COL_SIZE_N, 90),
        ("Modified", COL_MODIFIED, COL_MTIME_N, 150),
    ] {
        let r = CellRendererText::new();
        r.set_property("ellipsize", gtk::pango::EllipsizeMode::End);
        // A name may contain a newline; draw it as a symbol on one line
        // instead of a second line that the fixed row height would clip.
        r.set_property("single-paragraph-mode", true);
        let c = TreeViewColumn::new();
        c.set_title(title);
        CellLayoutExt::pack_start(&c, &r, true);
        CellLayoutExt::add_attribute(&c, &r, "text", col as i32);
        c.set_resizable(true);
        c.set_sizing(gtk::TreeViewColumnSizing::Fixed);
        c.set_fixed_width(width);
        c.set_sort_column_id(sort as i32);
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
        paths: RefCell::new(Vec::new()),
        started: Cell::new(0),
        shown: Cell::new(0),
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
    {
        let ui = ui.clone();
        tree.connect_row_activated(move |tv, path, _| {
            // Enter on one of several selected rows opens all of them.
            if tv.selection().count_selected_rows() > 1 && tv.selection().path_is_selected(path) {
                open_selected(&ui);
            } else if let Some(p) = ui.store.iter(path).and_then(|it| path_at(&ui, &it)) {
                open(&p);
            }
        });
    }
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
                // Right-click selects the row under the pointer, so the menu
                // always acts on what was clicked -- unless that row is part
                // of a selection, which the menu should act on as a whole.
                if !tv.selection().path_is_selected(&path) {
                    tv.selection().unselect_all();
                    tv.selection().select_path(&path);
                }
                tv.grab_focus();
                menu.popup_at_pointer(None);
            }
            glib::Propagation::Stop
        });
    }
    {
        // Ctrl+C, Ctrl+X and Delete belong to the list: bound here rather than
        // to the window, so that in the search box they still edit the text.
        let ui = ui.clone();
        tree.connect_key_press_event(move |_, ev| {
            use gtk::gdk::keys::constants as key;
            let state = ev.state();
            let ctrl = state.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gtk::gdk::ModifierType::SHIFT_MASK);
            let k = ev.keyval();
            if k == key::Delete || k == key::KP_Delete {
                if shift {
                    delete_selected(&ui);
                } else {
                    trash_selected(&ui);
                }
            } else if ctrl && (k == key::c || k == key::C) {
                clip_selected(&ui, false);
            } else if ctrl && (k == key::x || k == key::X) {
                clip_selected(&ui, true);
            } else {
                return glib::Propagation::Proceed;
            }
            glib::Propagation::Stop
        });
    }
    {
        // Menu key / Shift+F10 on a selected row.
        let menu = context.clone();
        let ui = ui.clone();
        tree.connect_popup_menu(move |_| {
            if selected_paths(&ui).is_empty() {
                return false;
            }
            menu.popup_at_pointer(None);
            true
        });
    }
    {
        let ui = ui.clone();
        open_folder.connect_clicked(move |_| open_folders(&ui));
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

/// A menu item whose shortcut is handled by the results list, not by an
/// accelerator group: the label shows the key, the list decides when it acts.
fn action_hint(menu: &Menu, label: &str, spec: &str, f: impl Fn() + 'static) {
    let mi = MenuItem::with_mnemonic(label);
    let (key, mods) = gtk::accelerator_parse(spec);
    if let Some(l) = mi
        .child()
        .and_then(|c| c.downcast::<gtk::AccelLabel>().ok())
    {
        l.set_accel(key, mods);
    }
    mi.connect_activate(move |_| f());
    menu.append(&mi);
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
        action(&file, "_Open", None, move || open_selected(&ui));
    }
    {
        let ui = ui.clone();
        action(&file, "Open _With…", None, move || open_with(&ui));
    }
    {
        let ui = ui.clone();
        action(
            &file,
            "Open Containing _Folder",
            Some((accel, "<Control>Return")),
            move || open_folders(&ui),
        );
    }
    file.append(&SeparatorMenuItem::new());
    {
        let ui = ui.clone();
        action(
            &file,
            "P_roperties…",
            Some((accel, "<Alt>Return")),
            move || show_properties(&ui),
        );
    }
    file.append(&SeparatorMenuItem::new());
    {
        let win = ui.window.clone();
        action(&file, "_Quit", Some((accel, "<Control>q")), move || {
            win.close()
        });
    }

    let edit = submenu(bar, "_Edit");
    {
        let ui = ui.clone();
        action_hint(&edit, "_Copy", "<Control>c", move || {
            clip_selected(&ui, false)
        });
    }
    {
        let ui = ui.clone();
        action_hint(&edit, "Cu_t", "<Control>x", move || {
            clip_selected(&ui, true)
        });
    }
    {
        let ui = ui.clone();
        action(
            &edit,
            "Copy _Path",
            Some((accel, "<Control><Shift>c")),
            move || copy_selected(&ui, false),
        );
    }
    {
        let ui = ui.clone();
        action(&edit, "Copy _Name", None, move || copy_selected(&ui, true));
    }
    edit.append(&SeparatorMenuItem::new());
    {
        let ui = ui.clone();
        action_hint(&edit, "Move to _Trash", "Delete", move || {
            trash_selected(&ui)
        });
    }
    {
        let ui = ui.clone();
        action_hint(
            &edit,
            "_Delete Permanently…",
            "<Shift>Delete",
            move || delete_selected(&ui),
        );
    }
    edit.append(&SeparatorMenuItem::new());
    {
        let ui = ui.clone();
        action(
            &edit,
            "_Preferences",
            Some((accel, "<Control>p")),
            move || show_preferences(&ui),
        );
    }

    // Same labels and shortcuts as FSearch. Everything calls "Search in Path"
    // "Match Path"; it is one option, not two.
    let search = submenu(bar, "_Search");
    {
        let ui = ui.clone();
        toggle(
            &search,
            "Match _Case",
            Some((accel, "<Control>i")),
            s.opts.match_case,
            move |on| set_opt(&ui, |o| o.match_case = on),
        );
    }
    {
        let ui = ui.clone();
        toggle(
            &search,
            "Enable _Regex",
            Some((accel, "<Control>r")),
            s.opts.regex,
            move |on| set_opt(&ui, |o| o.regex = on),
        );
    }
    {
        let ui = ui.clone();
        toggle(
            &search,
            "Search in _Path",
            Some((accel, "<Control>u")),
            s.opts.in_path,
            move |on| set_opt(&ui, |o| o.in_path = on),
        );
    }
    search.append(&SeparatorMenuItem::new());
    let all = RadioMenuItem::builder()
        .label("_All Results")
        .use_underline(true)
        .build();
    let files = RadioMenuItem::with_mnemonic_from_widget(&all, Some("_Files Only"));
    let folders = RadioMenuItem::with_mnemonic_from_widget(&all, Some("F_olders Only"));
    let radios = [
        (all, Kind::All),
        (files, Kind::Files),
        (folders, Kind::Folders),
    ];
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
        action(&help, "_Search Syntax", Some((accel, "F1")), move || {
            show_syntax(&ui)
        });
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
        action(&menu, "_Open", None, move || open_selected(&ui));
    }
    {
        let ui = ui.clone();
        action(&menu, "Open _With…", None, move || open_with(&ui));
    }
    {
        let ui = ui.clone();
        action(&menu, "Open Containing _Folder", None, move || {
            open_folders(&ui)
        });
    }
    menu.append(&SeparatorMenuItem::new());
    {
        let ui = ui.clone();
        action_hint(&menu, "_Copy", "<Control>c", move || {
            clip_selected(&ui, false)
        });
    }
    {
        let ui = ui.clone();
        action_hint(&menu, "Cu_t", "<Control>x", move || {
            clip_selected(&ui, true)
        });
    }
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
        action_hint(&menu, "Move to _Trash", "Delete", move || {
            trash_selected(&ui)
        });
    }
    {
        let ui = ui.clone();
        action_hint(
            &menu,
            "_Delete Permanently…",
            "<Shift>Delete",
            move || delete_selected(&ui),
        );
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

/// Searches run on a worker thread, so a slow one (a regex is a full scan:
/// ~150ms on names, ~450ms on paths at 2.3M files) never freezes the window.
/// Waiting for a pause in typing still saves sending a query per keystroke
/// that the next keystroke would supersede.
fn schedule_search(ui: &Rc<Ui>) {
    cancel_pending(ui);
    let delay = if ui.settings.borrow().opts.regex {
        300
    } else {
        40
    };
    let ui2 = ui.clone();
    let id = glib::timeout_add_local_once(Duration::from_millis(delay), move || {
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

/// One result, with what the worker learnt from stat(): (is_dir, size, mtime).
struct Row {
    raw: Vec<u8>,
    meta: Option<(bool, u64, i64)>,
}

fn run_search(ui: &Rc<Ui>) {
    let text = ui.entry.text().to_string();
    let pattern = text.trim().to_string();
    let s = ui.settings.borrow().clone();
    let seq = ui.started.get() + 1;
    ui.started.set(seq);
    if pattern.is_empty() {
        ui.shown.set(seq);
        clear_results(ui);
        ui.status
            .set_text(&format!("type to search{}", badges(&s.opts)));
        return;
    }

    // Say so only if the answer is slow in coming; saying it every time would
    // flash in the status line on each keystroke.
    {
        let ui = ui.clone();
        let badges = badges(&s.opts);
        glib::timeout_add_local_once(Duration::from_millis(150), move || {
            if ui.started.get() == seq && ui.shown.get() != seq {
                ui.status.set_text(&format!("searching…{}", badges));
            }
        });
    }

    let socket = ui.socket.clone();
    let ui = ui.clone();
    glib::MainContext::default().spawn_local(async move {
        let limit = s.max_results as usize;
        let opts = s.opts;
        let job = gio::spawn_blocking(move || {
            let t0 = Instant::now();
            let hits = crate::query::search(&socket, &pattern, limit, &opts);
            let elapsed = t0.elapsed();
            // stat() here too: up to 20,000 of them is real time on a cold cache.
            let rows = hits.map(|hits| {
                hits.into_iter()
                    .map(|raw| {
                        let meta = std::fs::symlink_metadata(OsStr::from_bytes(&raw))
                            .ok()
                            .map(|m| (m.is_dir(), m.size(), m.mtime()));
                        Row { raw, meta }
                    })
                    .collect::<Vec<_>>()
            });
            (rows, elapsed)
        });
        let Ok((rows, elapsed)) = job.await else {
            return;
        };
        if ui.started.get() != seq {
            return; // superseded while it ran
        }
        ui.shown.set(seq);
        show_results(&ui, rows, elapsed, &s);
    });
}

fn clear_results(ui: &Ui) {
    ui.store.clear();
    ui.paths.borrow_mut().clear();
}

fn show_results(ui: &Ui, rows: Result<Vec<Row>, String>, elapsed: Duration, s: &Settings) {
    let badges = badges(&s.opts);
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            clear_results(ui);
            ui.status.set_text(&format!("{}{}", e, badges));
            return;
        }
    };
    clear_results(ui);
    // Detached, the view does not redraw per inserted row.
    ui.tree.set_model(None::<&ListStore>);
    let n = rows.len();
    {
        let mut paths = ui.paths.borrow_mut();
        for row in rows {
            let path = String::from_utf8_lossy(&row.raw);
            let name = path.rsplit('/').next().unwrap_or(&path);
            let parent = match path.rfind('/') {
                Some(0) | None => "/",
                Some(i) => &path[..i],
            };
            let (size, size_n, modified, mtime_n) = match row.meta {
                Some((true, _, t)) => ("—".to_string(), -1i64, fmt_time(t), t),
                Some((false, len, t)) => (fmt_size(len), len as i64, fmt_time(t), t),
                None => ("?".to_string(), -1, String::new(), -1),
            };
            let id = paths.len() as u32;
            ui.store.insert_with_values(
                None,
                &[
                    (COL_NAME, &name),
                    (COL_PATH, &parent),
                    (COL_SIZE, &size.as_str()),
                    (COL_MODIFIED, &modified.as_str()),
                    (COL_ID, &id),
                    (COL_SIZE_N, &size_n),
                    (COL_MTIME_N, &mtime_n),
                ],
            );
            paths.push(row.raw);
        }
    }
    ui.tree.set_model(Some(&ui.store));
    let capped = if n >= s.max_results as usize {
        " (limit reached)"
    } else {
        ""
    };
    ui.status.set_text(&format!(
        "{} result{}{} in {:.1} ms{}",
        n,
        if n == 1 { "" } else { "s" },
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
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------- actions

fn path_at(ui: &Ui, iter: &gtk::TreeIter) -> Option<PathBuf> {
    let id = ui.store.value(iter, COL_ID as i32).get::<u32>().ok()?;
    let raw = ui.paths.borrow().get(id as usize)?.clone();
    Some(PathBuf::from(OsString::from_vec(raw)))
}

/// Every selected row, in the order shown. Shift-click and Ctrl-click can
/// select many, so this is what the actions work on.
fn selected_paths(ui: &Ui) -> Vec<PathBuf> {
    ui.tree
        .selection()
        .selected_rows()
        .0
        .iter()
        .filter_map(|p| ui.store.iter(p))
        .filter_map(|it| path_at(ui, &it))
        .collect()
}

/// Opening a whole screenful of files at once is usually a slip of the hand,
/// and every one of them starts a program, so ask first past this many.
const OPEN_WITHOUT_ASKING: usize = 10;

fn open_selected(ui: &Rc<Ui>) {
    let paths = selected_paths(ui);
    if paths.is_empty() {
        return;
    }
    if paths.len() > OPEN_WITHOUT_ASKING {
        let dialog = MessageDialog::new(
            Some(&ui.window),
            DialogFlags::MODAL | DialogFlags::DESTROY_WITH_PARENT,
            MessageType::Question,
            ButtonsType::YesNo,
            &format!("Open all {} selected files?", paths.len()),
        );
        dialog.connect_response(move |d, resp| {
            if resp == ResponseType::Yes {
                for p in &paths {
                    open(p);
                }
            }
            d.close();
        });
        dialog.show_all();
        return;
    }
    for p in &paths {
        open(p);
    }
}

/// One window per folder, however many files are selected inside it.
fn open_folders(ui: &Ui) {
    let mut opened: Vec<PathBuf> = Vec::new();
    for p in selected_paths(ui) {
        let dir = parent_dir(&p).to_path_buf();
        if !opened.contains(&dir) {
            open(&dir);
            opened.push(dir);
        }
    }
}

/// The clipboard takes text, so a name that is not UTF-8 is copied with its
/// stray bytes replaced -- the one place a raw name cannot survive.
fn copy_selected(ui: &Ui, name_only: bool) {
    let paths = selected_paths(ui);
    if paths.is_empty() {
        return;
    }
    // Several selected rows copy as one per line, which is what a shell or a
    // text editor expects to receive.
    let text = paths
        .iter()
        .map(|p| match p.file_name() {
            Some(n) if name_only => n.to_string_lossy().into_owned(),
            _ => p.to_string_lossy().into_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD).set_text(&text);
    ui.status.set_text(&if paths.len() == 1 {
        format!("copied {}", text)
    } else {
        format!(
            "copied {} {}",
            paths.len(),
            if name_only { "names" } else { "paths" }
        )
    });
}

fn open_with(ui: &Ui) {
    let paths = selected_paths(ui);
    let Some(first) = paths.first() else { return };
    // The dialog offers the applications that suit the first file; they are
    // then handed the whole selection.
    let files: Vec<gio::File> = paths.iter().map(gio::File::for_path).collect();
    let dialog = AppChooserDialog::new(
        Some(&ui.window),
        DialogFlags::MODAL | DialogFlags::DESTROY_WITH_PARENT,
        &gio::File::for_path(first),
    );
    dialog.connect_response(move |d, resp| {
        if resp == ResponseType::Ok {
            if let Some(app) = d.app_info() {
                let _ = app.launch(&files, None::<&gio::AppLaunchContext>);
            }
        }
        d.close();
    });
    dialog.show_all();
}

/// Puts the selected files on the clipboard for a file manager to paste, as a
/// copy or as a cut. There is no single standard, so all of the usual targets
/// are offered at once: GNOME's Files, KDE's Dolphin and the others each read
/// the one they know. The clipboard holds them only while this window is open,
/// the same as in any other application.
fn clip_selected(ui: &Ui, cut: bool) {
    let paths = selected_paths(ui);
    if paths.is_empty() {
        return;
    }
    let uris: Vec<String> = paths
        .iter()
        .map(|p| gio::File::for_path(p).uri().to_string())
        .collect();
    // "copy\n<uri>\n<uri>" is what GNOME and Dolphin both understand.
    let gnome = format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n"));
    let uri_list = uris
        .iter()
        .map(|u| format!("{}\r\n", u))
        .collect::<String>();
    let kde_cut = if cut { "1" } else { "0" }.to_string();
    let text = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");

    let targets = [
        TargetEntry::new("x-special/gnome-copied-files", TargetFlags::empty(), 0),
        TargetEntry::new("text/uri-list", TargetFlags::empty(), 1),
        TargetEntry::new("application/x-kde-cutselection", TargetFlags::empty(), 2),
        TargetEntry::new("UTF8_STRING", TargetFlags::empty(), 3),
    ];
    let ok = gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD).set_with_data(
        &targets,
        move |_, selection, info| {
            let (name, data) = match info {
                0 => ("x-special/gnome-copied-files", gnome.as_bytes()),
                1 => ("text/uri-list", uri_list.as_bytes()),
                2 => ("application/x-kde-cutselection", kde_cut.as_bytes()),
                _ => ("UTF8_STRING", text.as_bytes()),
            };
            selection.set(&gtk::gdk::Atom::intern(name), 8, data);
        },
    );
    ui.status.set_text(&if !ok {
        "could not reach the clipboard".to_string()
    } else {
        format!(
            "{} {} {} — paste in a file manager",
            if cut { "cut" } else { "copied" },
            paths.len(),
            if paths.len() == 1 { "file" } else { "files" }
        )
    });
}

/// Deletes for good, with no trash to fall back on, so it asks first and says
/// plainly what is about to go.
fn delete_selected(ui: &Rc<Ui>) {
    let paths = selected_paths(ui);
    if paths.is_empty() {
        return;
    }
    let folders = paths.iter().filter(|p| p.is_dir()).count();
    let what = match (paths.len(), folders) {
        (1, 0) => format!("Permanently delete “{}”?", name_of(&paths[0])),
        (1, _) => format!(
            "Permanently delete the folder “{}” and everything in it?",
            name_of(&paths[0])
        ),
        (n, 0) => format!("Permanently delete {} files?", n),
        (n, f) => format!(
            "Permanently delete {} items, including {} folder{} and everything in {}?",
            n,
            f,
            if f == 1 { "" } else { "s" },
            if f == 1 { "it" } else { "them" }
        ),
    };
    let dialog = MessageDialog::new(
        Some(&ui.window),
        DialogFlags::MODAL | DialogFlags::DESTROY_WITH_PARENT,
        MessageType::Warning,
        ButtonsType::YesNo,
        &what,
    );
    dialog.set_secondary_text(Some("This cannot be undone."));
    let ui = ui.clone();
    dialog.connect_response(move |d, resp| {
        d.close();
        if resp == ResponseType::Yes {
            remove_selected(&ui, true);
        }
    });
    dialog.show_all();
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .unwrap_or(p.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// Opens the file manager's own Properties dialog (Dolphin on KDE, Nautilus
/// on GNOME) through the freedesktop FileManager1 interface, the same one
/// FSearch uses. Dolphin only replies once that dialog is closed, so the call
/// is asynchronous with no timeout; only a real failure, such as no file
/// manager providing the interface, is reported.
fn show_properties(ui: &Rc<Ui>) {
    let paths = selected_paths(ui);
    if paths.is_empty() {
        return;
    }
    // gio percent-encodes the URI, so names with spaces or '#' survive. The
    // interface takes a list, so a multiple selection opens one dialog per
    // file, or a combined one, as the file manager sees fit.
    let uris: Vec<String> = paths
        .iter()
        .map(|p| gio::File::for_path(p).uri().to_string())
        .collect();
    let conn = match gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>) {
        Ok(c) => c,
        Err(e) => {
            ui.status
                .set_text(&format!("could not open Properties: {}", e.message()));
            return;
        }
    };
    let ui2 = ui.clone();
    conn.call(
        Some("org.freedesktop.FileManager1"),
        "/org/freedesktop/FileManager1",
        "org.freedesktop.FileManager1",
        "ShowItemProperties",
        Some(&(uris, "").to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        i32::MAX, // G_MAXINT: no timeout
        None::<&gio::Cancellable>,
        move |res| {
            if let Err(e) = res {
                ui2.status
                    .set_text(&format!("could not open Properties: {}", e.message()));
            }
        },
    );
}

/// Moves the selected file to the desktop trash (recoverable), and drops the
/// row. The daemon sees the move through fanotify on its own.
fn trash_selected(ui: &Ui) {
    remove_selected(ui, false);
}

/// Moves the selected files to the desktop trash (recoverable), or deletes
/// them outright, and drops their rows. The daemon sees the change through
/// fanotify on its own.
fn remove_selected(ui: &Ui, permanent: bool) {
    // Row references survive the removals: a plain path would point at the
    // wrong row as soon as an earlier one is gone.
    let refs: Vec<TreeRowReference> = ui
        .tree
        .selection()
        .selected_rows()
        .0
        .iter()
        .filter_map(|p| TreeRowReference::new(&ui.store, p))
        .collect();
    let (mut done, mut last, mut failure) = (0usize, None, None);
    for r in refs {
        let Some(iter) = r.path().and_then(|p| ui.store.iter(&p)) else {
            continue;
        };
        let Some(path) = path_at(ui, &iter) else {
            continue;
        };
        let result = if !permanent {
            gio::File::for_path(&path)
                .trash(None::<&gio::Cancellable>)
                .map_err(|e| e.message().to_string())
        } else if path.is_dir() {
            std::fs::remove_dir_all(&path).map_err(|e| e.to_string())
        } else {
            std::fs::remove_file(&path).map_err(|e| e.to_string())
        };
        match result {
            Ok(()) => {
                ui.store.remove(&iter);
                done += 1;
                last = Some(path);
            }
            Err(e) => failure = Some(e),
        }
    }
    let verb = if permanent {
        "deleted"
    } else {
        "moved to trash"
    };
    // One failure is worth reporting even when the rest went.
    ui.status.set_text(&match (done, failure) {
        (0, Some(e)) => format!("could not delete: {}", e),
        (n, Some(e)) => format!("{} {}; one failed: {}", verb, n, e),
        (1, None) => match last {
            Some(p) => format!("{}: {}", verb, p.display()),
            None => String::new(),
        },
        (n, None) => format!("{} {} files", verb, n),
    });
}

fn open(target: &Path) {
    let t = target.to_path_buf();
    // Reap the child, so opening files repeatedly does not accumulate zombies.
    std::thread::spawn(move || {
        if let Ok(mut c) = std::process::Command::new("xdg-open").arg(&t).spawn() {
            let _ = c.wait();
        }
    });
}

fn parent_dir(p: &Path) -> &Path {
    p.parent().unwrap_or(Path::new("/"))
}

// ---------------------------------------------------------------- dialogs

fn show_preferences(ui: &Rc<Ui>) {
    let dialog = Dialog::with_buttons(
        Some("Preferences"),
        Some(&ui.window),
        DialogFlags::MODAL | DialogFlags::DESTROY_WITH_PARENT,
        &[("_Close", ResponseType::Close)],
    );
    dialog.set_default_width(600);
    let s = ui.settings.borrow().clone();

    let content = GtkBox::new(Orientation::Vertical, 8);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let sayt = CheckButton::with_mnemonic("Search as you _type");
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
    content.pack_start(&sayt, false, false, 0);
    content.pack_start(&hidden, false, false, 0);
    content.pack_start(&limit_row, false, false, 0);

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

    // Which folders the daemon indexes. They belong to the machine, not to
    // this user, so applying goes through pkexec and asks for an administrator.
    let (cfg, cfg_note) = match config::Config::load(config::DEFAULT_PATH) {
        Ok(Some(c)) => (c, None),
        Ok(None) => (config::Config::default(), None),
        Err(e) => (config::Config::default(), Some(e)),
    };
    let apply_status = Label::new(cfg_note.as_deref());
    apply_status.set_xalign(0.0);
    apply_status.set_line_wrap(true);
    let (roots_box, roots) = folder_list(&dialog, &apply_status, "Indexed folders", &cfg.roots);
    let (excl_box, exclude) = folder_list(&dialog, &apply_status, "Excluded folders", &cfg.exclude);
    let (rescan_box, rescan) = folder_list(&dialog, &apply_status, "Network folders", &cfg.rescan);
    let every_row = GtkBox::new(Orientation::Horizontal, 8);
    let every_label = Label::with_mnemonic("Walk network folders every (_minutes):");
    let every = SpinButton::with_range(1.0, 10_080.0, 5.0);
    every.set_value((cfg.rescan_interval / 60).max(1) as f64);
    every_label.set_mnemonic_widget(Some(&every));
    every_row.pack_start(&every_label, false, false, 0);
    every_row.pack_start(&every, false, false, 0);
    let apply = Button::with_mnemonic("_Apply Folder Changes…");
    let apply_row = GtkBox::new(Orientation::Horizontal, 8);
    apply_row.pack_start(&apply_status, true, true, 0);
    apply_row.pack_end(&apply, false, false, 0);
    content.pack_start(
        &gtk::Separator::new(Orientation::Horizontal),
        false,
        false,
        6,
    );
    for w in [&roots_box, &excl_box, &rescan_box] {
        content.pack_start(w, true, true, 0);
    }
    content.pack_start(&every_row, false, false, 0);
    content.pack_start(&apply_row, false, false, 0);
    {
        let ui = ui.clone();
        let st = apply_status.clone();
        apply.connect_clicked(move |btn| {
            let c = config::Config {
                roots: store_paths(&roots),
                exclude: store_paths(&exclude),
                rescan: store_paths(&rescan),
                rescan_interval: every.value_as_int().max(1) as u64 * 60,
            };
            // Check here first, so a mistake does not cost a password prompt.
            if let Err(e) = c.clone().validate() {
                st.set_text(&e);
                return;
            }
            st.set_text("waiting for authentication…");
            btn.set_sensitive(false);
            let text = c.to_text();
            let (st, btn, ui) = (st.clone(), btn.clone(), ui.clone());
            glib::MainContext::default().spawn_local(async move {
                let res = gio::spawn_blocking(move || run_configure(&text)).await;
                btn.set_sensitive(true);
                match res.unwrap_or_else(|_| Err("the settings helper failed".into())) {
                    Ok(msg) => {
                        st.set_text(&msg);
                        // The daemon restarts; report on it once it is back.
                        glib::timeout_add_local_once(Duration::from_secs(3), move || {
                            show_connection(&ui)
                        });
                    }
                    Err(e) => st.set_text(&e),
                }
            });
        });
    }

    dialog.content_area().pack_start(&content, true, true, 0);
    dialog.connect_response(|d, _| d.close());
    dialog.show_all();
}

/// An editable list of folders for the Preferences dialog.
fn folder_list(
    parent: &Dialog,
    status: &Label,
    title: &str,
    paths: &[String],
) -> (GtkBox, ListStore) {
    let store = ListStore::new(&[String::static_type()]);
    for p in paths {
        store.insert_with_values(None, &[(0, p)]);
    }
    let tree = TreeView::with_model(&store);
    tree.set_headers_visible(false);
    let r = CellRendererText::new();
    r.set_property("ellipsize", gtk::pango::EllipsizeMode::Middle);
    let c = TreeViewColumn::new();
    CellLayoutExt::pack_start(&c, &r, true);
    CellLayoutExt::add_attribute(&c, &r, "text", 0);
    tree.append_column(&c);
    let scroll = ScrolledWindow::builder()
        .min_content_height(64)
        .hscrollbar_policy(PolicyType::Never)
        .build();
    scroll.set_shadow_type(gtk::ShadowType::In);
    scroll.add(&tree);

    let add = Button::with_label("Add…");
    let remove = Button::with_label("Remove");
    {
        let (store, parent, status) = (store.clone(), parent.clone(), status.clone());
        let title = title.to_string();
        add.connect_clicked(move |_| {
            let chooser = FileChooserDialog::with_buttons(
                Some(&format!("Add to {}", title)),
                Some(&parent),
                FileChooserAction::SelectFolder,
                &[
                    ("_Cancel", ResponseType::Cancel),
                    ("_Add", ResponseType::Accept),
                ],
            );
            chooser.set_modal(true);
            let (store, status) = (store.clone(), status.clone());
            chooser.connect_response(move |d, resp| {
                if resp == ResponseType::Accept {
                    match d.filename().as_deref().and_then(|p| p.to_str()) {
                        Some(p) => {
                            store.insert_with_values(None, &[(0, &p)]);
                        }
                        None => status.set_text("folder names must be valid UTF-8"),
                    }
                }
                d.close();
            });
            chooser.show_all();
        });
    }
    {
        let (tree, store) = (tree.clone(), store.clone());
        remove.connect_clicked(move |_| {
            if let Some((_, iter)) = tree.selection().selected() {
                store.remove(&iter);
            }
        });
    }
    let buttons = GtkBox::new(Orientation::Vertical, 4);
    buttons.pack_start(&add, false, false, 0);
    buttons.pack_start(&remove, false, false, 0);
    let row = GtkBox::new(Orientation::Horizontal, 6);
    row.pack_start(&scroll, true, true, 0);
    row.pack_start(&buttons, false, false, 0);
    let label = Label::new(None);
    label.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(title)));
    label.set_xalign(0.0);
    let v = GtkBox::new(Orientation::Vertical, 4);
    v.pack_start(&label, false, false, 0);
    v.pack_start(&row, true, true, 0);
    (v, store)
}

fn store_paths(store: &ListStore) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(it) = store.iter_first() {
        loop {
            if let Ok(p) = store.value(&it, 0).get::<String>() {
                out.push(p);
            }
            if !store.iter_next(&it) {
                break;
            }
        }
    }
    out
}

/// Runs `pkexec spoor configure -` with the new settings on its stdin, and
/// turns the outcome into a sentence for the dialog.
fn run_configure(text: &str) -> Result<String, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // Replaced on disk while running (an upgrade): the new file is at the old path.
    let exe = PathBuf::from(exe.to_string_lossy().trim_end_matches(" (deleted)"));
    let mut child = Command::new("pkexec")
        .arg(&exe)
        .args(["configure", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run pkexec: {}", e))?;
    if let Some(mut w) = child.stdin.take() {
        let _ = w.write_all(text.as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    match out.status.code() {
        Some(0) => Ok(stdout),
        Some(126) => Err("Cancelled; the folders are unchanged.".into()),
        Some(127) => Err("Not authorized; the folders are unchanged.".into()),
        _ if stderr.is_empty() => Err("Could not save the folders.".into()),
        _ => Err(stderr.trim_start_matches("spoor: ").to_string()),
    }
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
        .comments("Instant file search for Linux.")
        .logo_icon_name("spoor")
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
