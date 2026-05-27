//! Buoy — Linux GTK4 client.
//!
//! Depends on `buoy-core` directly (Rust -> Rust, no FFI). The UI mirrors
//! the iOS/macOS apps: a list of thoughts with newest at the bottom, and a
//! composer below the divider that submits on Enter or on clicking Save.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use buoy_core::{Thought, ThoughtStore};
use gtk::glib;
use gtk::prelude::*;
use gtk::{
    Application, ApplicationWindow, Box as GtkBox, Button, Entry, Label, ListBox, ListBoxRow,
    Orientation, ScrolledWindow, Separator,
};

const APP_ID: &str = "io.joemafrici.Buoy";

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &Application) {
    let store = match open_store() {
        Ok(s) => Rc::new(s),
        Err(err) => {
            eprintln!("buoy: failed to open store: {err}");
            std::process::exit(1);
        }
    };

    let list_box = ListBox::new();
    list_box.set_selection_mode(gtk::SelectionMode::None);

    let scrolled = ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&list_box)
        .build();

    let entry = Entry::builder()
        .placeholder_text("What's on your mind?")
        .hexpand(true)
        .build();

    let save_button = Button::with_label("Save");
    save_button.add_css_class("suggested-action");

    let composer = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    composer.append(&entry);
    composer.append(&save_button);

    let main_box = GtkBox::new(Orientation::Vertical, 0);
    main_box.append(&scrolled);
    main_box.append(&Separator::new(Orientation::Horizontal));
    main_box.append(&composer);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Buoy")
        .default_width(640)
        .default_height(800)
        .child(&main_box)
        .build();

    populate_list(&list_box, &store);
    scroll_to_bottom(&scrolled);

    let save = {
        let store = Rc::clone(&store);
        let list_box = list_box.clone();
        let entry = entry.clone();
        let scrolled = scrolled.clone();
        move || {
            let text = entry.text();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return;
            }
            if let Err(err) = store.create(trimmed) {
                eprintln!("buoy: save failed: {err}");
                return;
            }
            entry.set_text("");
            populate_list(&list_box, &store);
            scroll_to_bottom(&scrolled);
        }
    };

    let save_for_button = save.clone();
    save_button.connect_clicked(move |_| save_for_button());
    entry.connect_activate(move |_| save());

    window.present();
    entry.grab_focus();
}

fn populate_list(list_box: &ListBox, store: &ThoughtStore) {
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }

    let thoughts = match store.list() {
        Ok(t) => t,
        Err(err) => {
            eprintln!("buoy: list failed: {err}");
            return;
        }
    };

    // Core returns newest-first; reverse for newest-at-bottom UX.
    for thought in thoughts.into_iter().rev() {
        list_box.append(&make_row(&thought));
    }
}

fn make_row(thought: &Thought) -> ListBoxRow {
    let label = Label::builder()
        .label(&thought.text)
        .wrap(true)
        .xalign(0.0)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(6)
        .build();

    let row = ListBoxRow::new();
    row.set_child(Some(&label));
    row.set_selectable(false);
    row.set_activatable(false);
    row
}

fn scroll_to_bottom(scrolled: &ScrolledWindow) {
    // The adjustment's `upper` only reflects the new content after GTK has
    // laid it out, so defer to the next idle tick before jumping.
    let adjust = scrolled.vadjustment();
    glib::idle_add_local(move || {
        adjust.set_value(adjust.upper());
        glib::ControlFlow::Break
    });
}

fn open_store() -> Result<ThoughtStore, Box<dyn std::error::Error>> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(ThoughtStore::open(&dir.join("buoy.sqlite"))?)
}

fn data_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return Path::new(&xdg).join("buoy");
    }
    let home = std::env::var_os("HOME").expect("$HOME is unset");
    Path::new(&home).join(".local").join("share").join("buoy")
}
