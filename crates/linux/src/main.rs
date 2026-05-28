//! Buoy — Linux GTK4 client.
//!
//! Depends on `buoy-core` directly (Rust -> Rust, no FFI). The UI mirrors
//! the iOS/macOS apps: a list of thoughts with newest at the bottom, and a
//! composer below the divider that submits on Enter or on clicking Save.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

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
    let text = Label::builder()
        .label(&thought.text)
        .wrap(true)
        .xalign(0.0)
        .build();

    let timestamp = Label::builder()
        .label(format_relative(thought.created_at, now_unix_millis()))
        .xalign(0.0)
        .build();
    timestamp.add_css_class("caption");
    timestamp.add_css_class("dim-label");

    let row_box = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(2)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    row_box.append(&text);
    row_box.append(&timestamp);

    let row = ListBoxRow::new();
    row.set_child(Some(&row_box));
    row.set_selectable(false);
    row.set_activatable(false);
    row
}

/// Format `created_ms` as a relative time string (e.g. "5 minutes ago")
/// against `now_ms`. Negative deltas (future timestamps) are rendered as
/// "just now"; that should never happen in practice but is the least
/// surprising fallback if it does.
fn format_relative(created_ms: i64, now_ms: i64) -> String {
    let delta_sec = (now_ms - created_ms) / 1000;
    if delta_sec < 60 {
        return "just now".into();
    }
    let (value, unit) = if delta_sec < 3_600 {
        (delta_sec / 60, "minute")
    } else if delta_sec < 86_400 {
        (delta_sec / 3_600, "hour")
    } else if delta_sec < 86_400 * 7 {
        (delta_sec / 86_400, "day")
    } else if delta_sec < 86_400 * 30 {
        (delta_sec / (86_400 * 7), "week")
    } else if delta_sec < 86_400 * 365 {
        (delta_sec / (86_400 * 30), "month")
    } else {
        (delta_sec / (86_400 * 365), "year")
    };
    let plural = if value == 1 { "" } else { "s" };
    format!("{value} {unit}{plural} ago")
}

fn now_unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use super::format_relative;

    const SEC: i64 = 1_000;
    const MIN: i64 = 60 * SEC;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;

    fn at(now_ms: i64, age_ms: i64) -> String {
        format_relative(now_ms - age_ms, now_ms)
    }

    #[test]
    fn under_a_minute_is_just_now() {
        assert_eq!(at(1_000_000, 0), "just now");
        assert_eq!(at(1_000_000, 30 * SEC), "just now");
        assert_eq!(at(1_000_000, 59 * SEC), "just now");
    }

    #[test]
    fn minutes_hours_days_weeks_months_years() {
        assert_eq!(at(1_000_000_000, MIN), "1 minute ago");
        assert_eq!(at(1_000_000_000, 5 * MIN), "5 minutes ago");
        assert_eq!(at(1_000_000_000, HOUR), "1 hour ago");
        assert_eq!(at(1_000_000_000, 3 * HOUR), "3 hours ago");
        assert_eq!(at(1_000_000_000, DAY), "1 day ago");
        assert_eq!(at(1_000_000_000, 3 * DAY), "3 days ago");
        assert_eq!(at(1_000_000_000, WEEK), "1 week ago");
        assert_eq!(at(1_000_000_000, 2 * WEEK), "2 weeks ago");
        assert_eq!(at(1_000_000_000, 45 * DAY), "1 month ago");
        assert_eq!(at(10_000_000_000, 400 * DAY), "1 year ago");
    }

    #[test]
    fn future_timestamps_render_as_just_now() {
        // Should never happen with monotonic capture, but the fallback
        // must be benign rather than panic or render a negative count.
        assert_eq!(at(1_000_000, -(5 * MIN)), "just now");
    }
}
