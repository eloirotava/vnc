//! Test fixture for rvnc: paints four known colour quadrants, animates a
//! square across the middle, and reacts to input so the XTEST path can be
//! checked end to end.
//!
//!     cargo run --example xdraw        (uses $DISPLAY)
//!
//! Markers it paints on input:
//!   * magenta square at the top-left  — a pointer button was pressed
//!   * cyan square next to it          — a key was pressed
//! Every key press is also logged with its keysym.

use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ChangeGCAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask,
    PropMode, SelectionNotifyEvent, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;

/// Top-left, top-right, bottom-left, bottom-right.
const QUADRANTS: [u32; 4] = [0xff0000, 0x00ff00, 0x0000ff, 0xffffff];
const SQUARE: u16 = 40;
const MARKER: u16 = 24;
const BUTTON_MARKER: u32 = 0xff00ff;
const KEY_MARKER: u32 = 0x00ffff;

struct App {
    conn: x11rb::rust_connection::RustConnection,
    win: xproto::Window,
    gc: xproto::Gcontext,
    w: u16,
    h: u16,
}

impl App {
    /// Paint the four quadrants over the whole current screen.
    fn repaint(&self) -> Result<(), Box<dyn std::error::Error>> {
        let (hw, hh) = (self.w / 2, self.h / 2);
        for (i, colour) in QUADRANTS.iter().enumerate() {
            let (x, y) = ((i as i16 % 2) * hw as i16, (i as i16 / 2) * hh as i16);
            // The right and bottom halves take any odd pixel.
            let width = if i % 2 == 0 { hw } else { self.w - hw };
            let height = if i / 2 == 0 { hh } else { self.h - hh };
            self.fill(x, y, width, height, *colour)?;
        }
        self.conn.sync()?;
        Ok(())
    }
}

impl App {
    fn fill(&self, x: i16, y: i16, w: u16, h: u16, colour: u32) -> Result<(), Box<dyn std::error::Error>> {
        self.conn.change_gc(self.gc, &ChangeGCAux::new().foreground(colour))?;
        self.conn.poly_fill_rectangle(
            self.win,
            self.gc,
            &[xproto::Rectangle { x, y, width: w, height: h }],
        )?;
        Ok(())
    }

    /// Repaint a region with the quadrant colours underneath it, so the
    /// animation leaves no trail.
    fn restore(&self, x: i16, y: i16, w: u16, h: u16) -> Result<(), Box<dyn std::error::Error>> {
        let (hw, hh) = ((self.w / 2) as i16, (self.h / 2) as i16);
        for (i, colour) in QUADRANTS.iter().enumerate() {
            let qx = (i as i16 % 2) * hw;
            let qy = (i as i16 / 2) * hh;
            let x0 = x.max(qx);
            let y0 = y.max(qy);
            let x1 = (x + w as i16).min(qx + hw);
            let y1 = (y + h as i16).min(qy + hh);
            if x1 > x0 && y1 > y0 {
                self.fill(x0, y0, (x1 - x0) as u16, (y1 - y0) as u16, *colour)?;
            }
        }
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (conn, screen_num) = x11rb::rust_connection::RustConnection::connect(None)?;
    let screen = conn.setup().roots[screen_num].clone();
    let (w, h) = (screen.width_in_pixels, screen.height_in_pixels);
    let (min_keycode, max_keycode) = (conn.setup().min_keycode, conn.setup().max_keycode);

    let win = conn.generate_id()?;
    conn.create_window(
        screen.root_depth,
        win,
        screen.root,
        0,
        0,
        w,
        h,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .override_redirect(1)
            .background_pixel(0x101010)
            .event_mask(EventMask::KEY_PRESS | EventMask::BUTTON_PRESS),
    )?;
    conn.map_window(win)?;
    conn.set_input_focus(xproto::InputFocus::PARENT, win, x11rb::CURRENT_TIME)?;
    // Watch for the screen changing size under us.
    conn.change_window_attributes(
        screen.root,
        &xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
    )?;

    let atom = |name: &str| -> Result<Atom, Box<dyn std::error::Error>> {
        Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
    };
    let clipboard_atom = atom("CLIPBOARD")?;
    let utf8_atom = atom("UTF8_STRING")?;
    let prop_atom = atom("XDRAW_SEL")?;
    let targets_atom = atom("TARGETS")?;
    let mut owned_text = String::from("hello-from-the-desktop");

    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new())?;
    let mut app = App { conn, win, gc, w, h };
    app.repaint()?;
    eprintln!("xdraw: painted {w}x{h}");

    // Offer something on the clipboard so the browser side can be checked.
    app.conn
        .set_selection_owner(win, clipboard_atom, x11rb::CURRENT_TIME)?;
    app.conn.flush()?;
    let mut we_own_clipboard = true;
    let mut last_seen = String::new();
    let mut poll_countdown = 0;

    let keymap = app
        .conn
        .get_keyboard_mapping(min_keycode, max_keycode - min_keycode + 1)?
        .reply()?;

    let mut step = 0i16;
    let mut previous: Option<i16> = None;

    loop {
        let y = (app.h / 2) as i16 - SQUARE as i16 / 2;
        while let Some(event) = app.conn.poll_for_event()? {
            match event {
                Event::ConfigureNotify(e) if e.window == screen.root => {
                    if (e.width, e.height) != (app.w, app.h) {
                        eprintln!("xdraw: screen resized to {}x{}", e.width, e.height);
                        app.w = e.width;
                        app.h = e.height;
                        app.conn.configure_window(
                            win,
                            &xproto::ConfigureWindowAux::new()
                                .width(e.width as u32)
                                .height(e.height as u32),
                        )?;
                        app.repaint()?;
                        previous = None;
                        step = 0;
                    }
                }
                Event::SelectionClear(_) => {
                    we_own_clipboard = false;
                    eprintln!("xdraw: lost the clipboard");
                }
                Event::SelectionRequest(e) => {
                    let mut property = e.property;
                    if e.target == targets_atom {
                        app.conn.change_property32(
                            PropMode::REPLACE,
                            e.requestor,
                            property,
                            AtomEnum::ATOM,
                            &[targets_atom, utf8_atom, Atom::from(AtomEnum::STRING)],
                        )?;
                    } else if e.target == utf8_atom || e.target == Atom::from(AtomEnum::STRING) {
                        app.conn.change_property8(
                            PropMode::REPLACE,
                            e.requestor,
                            property,
                            e.target,
                            owned_text.as_bytes(),
                        )?;
                    } else {
                        property = x11rb::NONE;
                    }
                    app.conn.send_event(
                        false,
                        e.requestor,
                        EventMask::NO_EVENT,
                        SelectionNotifyEvent {
                            response_type: SELECTION_NOTIFY_EVENT,
                            sequence: 0,
                            time: e.time,
                            requestor: e.requestor,
                            selection: e.selection,
                            target: e.target,
                            property,
                        },
                    )?;
                    app.conn.flush()?;
                    eprintln!("xdraw: served the clipboard to another client");
                }
                Event::SelectionNotify(e) => {
                    if e.property != x11rb::NONE {
                        let r = app
                            .conn
                            .get_property(true, win, prop_atom, AtomEnum::ANY, 0, 4096)?
                            .reply()?;
                        let text = String::from_utf8_lossy(&r.value).into_owned();
                        if !text.is_empty() && text != last_seen {
                            last_seen = text.clone();
                            eprintln!("xdraw: clipboard now holds {text:?}");
                        }
                    }
                }
                Event::ButtonPress(e) => {
                    eprintln!("xdraw: button {} at {},{}", e.detail, e.event_x, e.event_y);
                    app.fill(0, 0, MARKER, MARKER, BUTTON_MARKER)?;
                }
                Event::KeyPress(e) => {
                    let per = keymap.keysyms_per_keycode as usize;
                    let index = (e.detail - min_keycode) as usize * per;
                    let keysym = keymap.keysyms.get(index).copied().unwrap_or(0);
                    eprintln!("xdraw: key keycode {} keysym 0x{keysym:04x}", e.detail);
                    app.fill(MARKER as i16 + 8, 0, MARKER, MARKER, KEY_MARKER)?;
                }
                _ => {}
            }
        }

        // Once something else owns the clipboard, keep an eye on it.
        if !we_own_clipboard {
            poll_countdown -= 1;
            if poll_countdown <= 0 {
                poll_countdown = 5;
                app.conn.convert_selection(
                    win,
                    clipboard_atom,
                    utf8_atom,
                    prop_atom,
                    x11rb::CURRENT_TIME,
                )?;
            }
        }
        let _ = &mut owned_text;

        if let Some(old) = previous {
            app.restore(old, y, SQUARE, SQUARE)?;
        }
        app.fill(step, y, SQUARE, SQUARE, 0x000000)?;
        previous = Some(step);
        app.conn.flush()?;

        step = (step + 8) % (app.w as i16 - SQUARE as i16).max(1);
        std::thread::sleep(Duration::from_millis(100));
    }
}
