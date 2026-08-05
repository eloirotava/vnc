//! X selection bridge, so copy and paste work in both directions.
//!
//! An unmapped window owns CLIPBOARD and PRIMARY when the browser copies
//! something, and XFIXES tells us when anything else on the display takes
//! ownership so we can read the new text and forward it.

use std::sync::{Arc, Mutex};

use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
    SelectionNotifyEvent, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::CURRENT_TIME;

use crate::http::log;
use crate::x11::XResult;

/// Ignore anything larger than this; clipboards that big are a mistake.
const MAX_TEXT: usize = 1 << 20;

struct Atoms {
    clipboard: Atom,
    primary: Atom,
    targets: Atom,
    utf8_string: Atom,
    text: Atom,
    incr: Atom,
    property: Atom,
}

pub struct Clipboard {
    conn: RustConnection,
    window: Window,
    atoms: Atoms,
    /// The text we currently offer to X clients, set from the browser side.
    ours: Mutex<String>,
}

impl Clipboard {
    pub fn open(display: Option<&str>) -> XResult<Arc<Self>> {
        let (conn, screen_num) = RustConnection::connect(display)?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;

        let window = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;

        let intern = |name: &str| -> XResult<Atom> {
            Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
        };
        let atoms = Atoms {
            clipboard: intern("CLIPBOARD")?,
            primary: Atom::from(AtomEnum::PRIMARY),
            targets: intern("TARGETS")?,
            utf8_string: intern("UTF8_STRING")?,
            text: intern("TEXT")?,
            incr: intern("INCR")?,
            property: intern("RVNC_SELECTION")?,
        };

        // XFIXES tells us when someone else copies something.
        conn.xfixes_query_version(5, 0)?.reply()?;
        let mask = xfixes::SelectionEventMask::SET_SELECTION_OWNER
            | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
            | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE;
        conn.xfixes_select_selection_input(window, atoms.clipboard, mask)?;
        conn.xfixes_select_selection_input(window, atoms.primary, mask)?;
        conn.flush()?;

        let clipboard = Clipboard { conn, window, atoms, ours: Mutex::new(String::new()) };
        // Whatever is already on the clipboard predates us; ask for it now so
        // the first client does not start out with an empty one.
        clipboard.request(clipboard.atoms.clipboard)?;
        Ok(Arc::new(clipboard))
    }

    /// Ask the current owner of `selection` for its text.
    fn request(&self, selection: Atom) -> XResult<()> {
        self.conn.convert_selection(
            self.window,
            selection,
            self.atoms.utf8_string,
            self.atoms.property,
            CURRENT_TIME,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    /// Offer `text` to the X display, as if the user had copied it there.
    pub fn set(&self, text: String) -> XResult<()> {
        *self.ours.lock().unwrap() = text;
        self.conn
            .set_selection_owner(self.window, self.atoms.clipboard, CURRENT_TIME)?;
        self.conn
            .set_selection_owner(self.window, self.atoms.primary, CURRENT_TIME)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Event loop. `on_change` receives text copied by programs on the
    /// display; it never fires for text this process put there itself.
    pub fn run(self: Arc<Self>, on_change: impl Fn(String)) -> XResult<()> {
        loop {
            match self.conn.wait_for_event()? {
                Event::XfixesSelectionNotify(e) => {
                    if e.owner != self.window && e.owner != x11rb::NONE {
                        self.request(e.selection)?;
                    }
                }
                Event::SelectionNotify(e) => {
                    if e.property != x11rb::NONE {
                        if let Some(text) = self.read_property()? {
                            on_change(text);
                        }
                    }
                }
                Event::SelectionRequest(e) => self.answer_request(&e)?,
                Event::SelectionClear(_) => {
                    self.ours.lock().unwrap().clear();
                }
                _ => {}
            }
        }
    }

    fn read_property(&self) -> XResult<Option<String>> {
        let reply = self
            .conn
            .get_property(
                true,
                self.window,
                self.atoms.property,
                AtomEnum::ANY,
                0,
                (MAX_TEXT / 4) as u32,
            )?
            .reply()?;

        if reply.type_ == self.atoms.incr {
            // Huge selections arrive in chunks; not worth supporting.
            log::debug("clipboard: ignoring an INCR transfer");
            return Ok(None);
        }
        if reply.value.is_empty() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&reply.value).into_owned()))
    }

    /// Hand our text to a program on the display that asked for it.
    fn answer_request(&self, req: &xproto::SelectionRequestEvent) -> XResult<()> {
        let mut property = req.property;
        if property == x11rb::NONE {
            property = req.target; // obsolete clients leave this unset
        }

        if req.target == self.atoms.targets {
            let targets = [
                self.atoms.targets,
                self.atoms.utf8_string,
                self.atoms.text,
                Atom::from(AtomEnum::STRING),
            ];
            self.conn.change_property32(
                PropMode::REPLACE,
                req.requestor,
                property,
                AtomEnum::ATOM,
                &targets,
            )?;
        } else if req.target == self.atoms.utf8_string
            || req.target == self.atoms.text
            || req.target == Atom::from(AtomEnum::STRING)
        {
            let text = self.ours.lock().unwrap().clone();
            self.conn.change_property8(
                PropMode::REPLACE,
                req.requestor,
                property,
                req.target,
                text.as_bytes(),
            )?;
        } else {
            property = x11rb::NONE; // we cannot provide that format
        }

        let event = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: req.time,
            requestor: req.requestor,
            selection: req.selection,
            target: req.target,
            property,
        };
        self.conn
            .send_event(false, req.requestor, EventMask::NO_EVENT, event)?;
        self.conn.flush()?;
        Ok(())
    }
}

/// RFB carries clipboard text as Latin-1. Characters outside it cannot be
/// represented, so they become '?' rather than corrupting the stream.
pub fn to_latin1(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| if (c as u32) < 256 { c as u8 } else { b'?' })
        .collect()
}

pub fn from_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// Unused import guard: `Property` is only referenced by the event match in
/// older x11rb versions.
const _: Option<Property> = None;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin1_round_trips_western_text() {
        let text = "Olá, ação — çãé";
        let bytes = to_latin1(text);
        // The em dash is outside Latin-1 and degrades, the accents survive.
        assert_eq!(from_latin1(&bytes), "Olá, ação ? çãé");
    }

    #[test]
    fn latin1_is_byte_for_byte_for_ascii() {
        assert_eq!(to_latin1("hello"), b"hello");
        assert_eq!(from_latin1(b"hello"), "hello");
    }

    #[test]
    fn non_latin1_becomes_a_placeholder() {
        assert_eq!(to_latin1("日本"), b"??");
        assert_eq!(to_latin1("emoji 🎉"), b"emoji ?");
    }
}
