//! Buoy — Linux GTK4 client.
//!
//! Depends on `buoy-core` directly (Rust -> Rust, no FFI). The UI mirrors
//! the iOS/macOS apps: a list of thoughts with newest at the bottom, a
//! composer below the divider that submits on Enter or on clicking Save,
//! and an edit-mode banner that appears when a stream row has been
//! tapped to edit.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use buoy_core::{Cursor, DEFAULT_PAGE_SIZE, Thought, ThoughtStore};
use gtk::glib::{self, Propagation};
use gtk::prelude::*;
use gtk::{
    Application, ApplicationWindow, Box as GtkBox, Button, EventControllerKey, GestureClick, Image,
    Label, ListBox, ListBoxRow, Orientation, PropagationPhase, ScrolledWindow, Separator, TextView,
    WrapMode, gdk,
};
use uuid::Uuid;

const APP_ID: &str = "io.joemafrici.Buoy";

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

#[allow(clippy::too_many_lines)] // GTK UI construction is imperative and naturally long.
fn build_ui(app: &Application) {
    let store = match open_store() {
        Ok(s) => Rc::new(s),
        Err(err) => {
            eprintln!("buoy: failed to open store: {err}");
            std::process::exit(1);
        }
    };

    let editing_id: Rc<RefCell<Option<Uuid>>> = Rc::new(RefCell::new(None));

    // Pagination state for the stream. `next_cursor` points at the page
    // after the oldest loaded thought (None = fully loaded); `loaded_count`
    // is how many thoughts the list currently shows, so a refresh can cover
    // the same window; `loading_older` guards against re-entrant loads
    // while a prepend is still settling.
    let next_cursor: Rc<RefCell<Option<Cursor>>> = Rc::new(RefCell::new(None));
    let loaded_count: Rc<Cell<usize>> = Rc::new(Cell::new(0));
    let loading_older: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    let list_box = ListBox::new();
    list_box.set_selection_mode(gtk::SelectionMode::None);

    let stream_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&list_box)
        .build();

    let text_view = TextView::builder()
        .wrap_mode(WrapMode::WordChar)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .hexpand(true)
        .build();

    let composer_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .min_content_height(40)
        .max_content_height(140)
        .hexpand(true)
        .child(&text_view)
        .build();

    let save_button = Button::with_label("Save");
    save_button.add_css_class("suggested-action");

    // Edit-mode banner: hidden when not editing.
    let editing_banner = build_editing_banner();
    editing_banner.set_visible(false);

    let composer = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    composer.append(&composer_scroll);
    composer.append(&save_button);

    let main_box = GtkBox::new(Orientation::Vertical, 0);
    main_box.append(&stream_scroll);
    main_box.append(&Separator::new(Orientation::Horizontal));
    main_box.append(&editing_banner);
    main_box.append(&composer);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Buoy")
        .default_width(640)
        .default_height(800)
        .child(&main_box)
        .build();

    // Force-settle every live thought when the user closes the window.
    // Linux has no scene-phase model, so window close is the natural
    // settling point: it draws a clean line under "this capture session"
    // before the next one starts.
    {
        let store = Rc::clone(&store);
        window.connect_close_request(move |_| {
            if let Err(err) = store.settle_all_live() {
                eprintln!("buoy: settle on close failed: {err}");
            }
            Propagation::Proceed
        });
    }

    // start_editing: enter edit mode for the given thought.
    let start_editing = {
        let editing_id = Rc::clone(&editing_id);
        let text_view = text_view.clone();
        let save_button = save_button.clone();
        let editing_banner = editing_banner.clone();
        move |id: Uuid, text: String| {
            *editing_id.borrow_mut() = Some(id);
            text_view.buffer().set_text(&text);
            editing_banner.set_visible(true);
            save_button.set_label("Update");
            text_view.grab_focus();
        }
    };

    // cancel_editing: leave edit mode and clear the draft.
    let cancel_editing = {
        let editing_id = Rc::clone(&editing_id);
        let text_view = text_view.clone();
        let save_button = save_button.clone();
        let editing_banner = editing_banner.clone();
        move || {
            *editing_id.borrow_mut() = None;
            text_view.buffer().set_text("");
            editing_banner.set_visible(false);
            save_button.set_label("Save");
        }
    };

    // make_wired_row: build a stream row with its tap-to-edit gesture.
    // ListBoxRow's `activate` signal is unreliable for plain mouse clicks
    // in GTK4, so each row gets an explicit GestureClick that enters edit
    // mode for its own thought.
    let make_wired_row = {
        let start_editing = start_editing.clone();
        move |thought: &Thought| -> ListBoxRow {
            let row = make_row(thought);
            let click = GestureClick::new();
            let start = start_editing.clone();
            let id = thought.id;
            let text = thought.text.clone();
            click.connect_released(move |_, _, _, _| {
                start(id, text.clone());
            });
            row.add_controller(click);
            row
        }
    };

    // refresh_list: reload the stream from the newest thought, covering at
    // least the window that was already loaded so a refresh never silently
    // shrinks what the user can see.
    let refresh_list = {
        let list_box = list_box.clone();
        let store = Rc::clone(&store);
        let make_wired_row = make_wired_row.clone();
        let next_cursor = Rc::clone(&next_cursor);
        let loaded_count = Rc::clone(&loaded_count);
        move || {
            let target = loaded_count.get().max(DEFAULT_PAGE_SIZE);
            let mut thoughts = Vec::new();
            let mut cursor = None;
            loop {
                let page = match store.list_paginated(cursor, DEFAULT_PAGE_SIZE) {
                    Ok(p) => p,
                    Err(err) => {
                        eprintln!("buoy: list failed: {err}");
                        return;
                    }
                };
                thoughts.extend(page.thoughts);
                cursor = page.next_cursor;
                if cursor.is_none() || thoughts.len() >= target {
                    break;
                }
            }
            *next_cursor.borrow_mut() = cursor;
            loaded_count.set(thoughts.len());

            while let Some(child) = list_box.first_child() {
                list_box.remove(&child);
            }
            for thought in thoughts.into_iter().rev() {
                list_box.append(&make_wired_row(&thought));
            }
        }
    };

    // load_older: fetch the page after the oldest loaded thought and
    // prepend its rows, keeping the user's scroll position anchored.
    let load_older = {
        let list_box = list_box.clone();
        let store = Rc::clone(&store);
        let make_wired_row = make_wired_row.clone();
        let next_cursor = Rc::clone(&next_cursor);
        let loaded_count = Rc::clone(&loaded_count);
        let loading_older = Rc::clone(&loading_older);
        let stream_scroll = stream_scroll.clone();
        move || {
            if loading_older.get() {
                return;
            }
            let Some(cursor) = *next_cursor.borrow() else {
                return;
            };
            loading_older.set(true);
            let page = match store.list_paginated(Some(cursor), DEFAULT_PAGE_SIZE) {
                Ok(p) => p,
                Err(err) => {
                    eprintln!("buoy: loading older thoughts failed: {err}");
                    loading_older.set(false);
                    return;
                }
            };
            *next_cursor.borrow_mut() = page.next_cursor;
            loaded_count.set(loaded_count.get() + page.thoughts.len());

            keep_scroll_anchored(&stream_scroll);
            // The page is newest-first; prepending in that order walks the
            // final top-of-list order out to oldest-first.
            for thought in &page.thoughts {
                list_box.prepend(&make_wired_row(thought));
            }
            loading_older.set(false);
        }
    };

    // Pull in older pages as the user scrolls within a viewport's height
    // of the top. load_older itself is a no-op once the stream is fully
    // loaded or while a prepend is in flight.
    {
        let load_older = load_older.clone();
        let adjust = stream_scroll.vadjustment();
        adjust.connect_value_changed(move |adjust| {
            if adjust.value() < adjust.page_size() {
                load_older();
            }
        });
    }

    // save: commit the draft as either a new thought or an update to the
    // one currently being edited, then refresh.
    let save = {
        let store = Rc::clone(&store);
        let editing_id = Rc::clone(&editing_id);
        let text_view = text_view.clone();
        let stream_scroll = stream_scroll.clone();
        let refresh_list = refresh_list.clone();
        let cancel_editing = cancel_editing.clone();
        move || {
            let buffer = text_view.buffer();
            let raw = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), true)
                .to_string();
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return;
            }
            let current_edit = *editing_id.borrow();
            let result = match current_edit {
                Some(id) => store.update_thought(id, trimmed).map(|_| ()),
                None => store.create(trimmed).map(|_| ()),
            };
            if let Err(err) = result {
                eprintln!("buoy: save failed: {err}");
                return;
            }
            cancel_editing();
            refresh_list();
            scroll_to_bottom(&stream_scroll);
        }
    };

    refresh_list();
    scroll_to_bottom(&stream_scroll);

    // Save button.
    let save_for_button = save.clone();
    save_button.connect_clicked(move |_| save_for_button());

    // Cancel button inside the banner.
    if let Some(cancel_button) = editing_banner_cancel(&editing_banner) {
        let cancel = cancel_editing.clone();
        cancel_button.connect_clicked(move |_| cancel());
    }

    // Capture-phase key handler on the TextView:
    //   - bare Return saves
    //   - Shift+Return inserts a newline (falls through to default)
    //   - Escape cancels an in-progress edit
    let key_controller = EventControllerKey::new();
    key_controller.set_propagation_phase(PropagationPhase::Capture);
    let save_for_key = save.clone();
    let cancel_for_key = cancel_editing.clone();
    let editing_id_for_key = Rc::clone(&editing_id);
    key_controller.connect_key_pressed(move |_, key, _, modifiers| {
        if key == gdk::Key::Escape && editing_id_for_key.borrow().is_some() {
            cancel_for_key();
            return Propagation::Stop;
        }
        let is_return = key == gdk::Key::Return || key == gdk::Key::KP_Enter;
        if is_return && !modifiers.contains(gdk::ModifierType::SHIFT_MASK) {
            save_for_key();
            return Propagation::Stop;
        }
        Propagation::Proceed
    });
    text_view.add_controller(key_controller);

    window.present();
    text_view.grab_focus();
}

fn make_row(thought: &Thought) -> ListBoxRow {
    let text = Label::builder()
        .label(&thought.text)
        .wrap(true)
        .xalign(0.0)
        .build();

    let relative = format_relative(thought.created_at, now_unix_millis());
    // Live thoughts get a leading bullet next to the timestamp. Settled
    // thoughts use the default caption styling.
    let timestamp_text = if thought.is_settled {
        relative
    } else {
        format!("• {relative}")
    };
    let timestamp = Label::builder().label(timestamp_text).xalign(0.0).build();
    timestamp.add_css_class("caption");
    timestamp.add_css_class("dim-label");
    if !thought.is_settled {
        timestamp.add_css_class("accent");
    }

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
    // We don't want the row's own activation behavior; clicks are handled
    // by a per-row GestureClick controller installed at refresh time.
    row.set_activatable(false);
    row
}

fn build_editing_banner() -> GtkBox {
    let icon = Image::from_icon_name("document-edit-symbolic");
    icon.add_css_class("dim-label");

    let label = Label::builder()
        .label("Editing thought")
        .xalign(0.0)
        .hexpand(true)
        .build();
    label.add_css_class("caption");
    label.add_css_class("dim-label");

    let cancel = Button::with_label("Cancel");
    cancel.add_css_class("flat");
    // Tag the cancel button so we can find it again from outside.
    cancel.set_widget_name("editing-cancel");

    let banner = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    banner.append(&icon);
    banner.append(&label);
    banner.append(&cancel);
    banner
}

fn editing_banner_cancel(banner: &GtkBox) -> Option<Button> {
    let mut child = banner.first_child();
    while let Some(widget) = child {
        if widget.widget_name() == "editing-cancel" {
            return widget.downcast::<Button>().ok();
        }
        child = widget.next_sibling();
    }
    None
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

/// Keep the viewport visually still across a row prepend. GTK preserves the
/// adjustment *value* (distance from the top of the content), so growing the
/// content above the viewport would visually jump the stream. Capture the
/// current position and re-apply it relative to the new upper bound once the
/// resize lands, then disconnect.
fn keep_scroll_anchored(scrolled: &ScrolledWindow) {
    let adjust = scrolled.vadjustment();
    let old_upper = adjust.upper();
    let old_value = adjust.value();
    let handler: Rc<RefCell<Option<glib::SignalHandlerId>>> = Rc::new(RefCell::new(None));
    let handler_in_closure = Rc::clone(&handler);
    let id = adjust.connect_changed(move |adjust| {
        let delta = adjust.upper() - old_upper;
        if delta <= 0.0 {
            return;
        }
        adjust.set_value(old_value + delta);
        if let Some(id) = handler_in_closure.borrow_mut().take() {
            adjust.disconnect(id);
        }
    });
    *handler.borrow_mut() = Some(id);
}

fn scroll_to_bottom(scrolled: &ScrolledWindow) {
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
        assert_eq!(at(1_000_000, -(5 * MIN)), "just now");
    }
}
