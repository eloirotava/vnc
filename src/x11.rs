//! X11 backend: screen capture through XDAMAGE/GetImage, pointer and
//! keyboard injection through XTEST, cursor shapes through XFIXES.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, ImageFormat, ImageOrder};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::CURRENT_TIME;

use crate::http::log;
use crate::screen::{CursorImage, Frame, Hub, Rect};

pub type XResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Outcome of a screen grab.
///
/// Resizing is the one thing that reliably makes `GetImage` fail: the request
/// is built from rvnc's idea of the screen size, and another thread can change
/// the real one in between. That arrives as a protocol error rather than a
/// broken connection, and it must not be fatal — a desktop that disappears
/// because its window was resized is worse than a dropped frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grab {
    Done,
    Raced,
}

/// How the X server lays out pixels in `GetImage` replies.
#[derive(Clone, Copy)]
struct ImageFmt {
    bits_per_pixel: u8,
    scanline_pad: u8,
    big_endian: bool,
    red: Channel,
    green: Channel,
    blue: Channel,
    /// The common case: 32bpp little-endian BGRX, copied without conversion.
    fast_bgrx: bool,
}

#[derive(Clone, Copy)]
struct Channel {
    shift: u32,
    max: u32,
}

impl Channel {
    fn new(mask: u32) -> Self {
        if mask == 0 {
            return Channel { shift: 0, max: 1 };
        }
        let shift = mask.trailing_zeros();
        Channel { shift, max: mask >> shift }
    }

    #[inline]
    fn take(&self, raw: u32) -> u32 {
        let v = (raw >> self.shift) & self.max;
        if self.max == 255 {
            v
        } else {
            (v * 255 + self.max / 2) / self.max
        }
    }
}

/// Capture side: owns its own X connection and the damage object.
pub struct Capture {
    conn: RustConnection,
    root: xproto::Window,
    pub width: u16,
    pub height: u16,
    fmt: ImageFmt,
    damage: Option<damage::Damage>,
    region: Option<xfixes::Region>,
    pub has_xfixes: bool,
}

impl Capture {
    pub fn open(display: Option<&str>) -> XResult<Self> {
        let (conn, screen_num) = RustConnection::connect(display)?;
        let setup = conn.setup();
        let screen = &setup.roots[screen_num];
        let root = screen.root;
        let (width, height) = (screen.width_in_pixels, screen.height_in_pixels);

        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|d| d.visuals.iter().map(move |v| (d.depth, v)))
            .find(|(_, v)| v.visual_id == screen.root_visual)
            .ok_or("root visual not found in the server setup")?;
        let (depth, visual) = visual;

        let pixmap_fmt = setup
            .pixmap_formats
            .iter()
            .find(|f| f.depth == depth)
            .ok_or("no pixmap format for the root depth")?;

        if !matches!(pixmap_fmt.bits_per_pixel, 16 | 24 | 32) {
            return Err(format!(
                "unsupported X server pixel size: {} bits (need 16, 24 or 32)",
                pixmap_fmt.bits_per_pixel
            )
            .into());
        }
        if visual.red_mask == 0 {
            return Err("the root visual is not true colour (palette displays are not supported)".into());
        }

        let big_endian = setup.image_byte_order == ImageOrder::MSB_FIRST;
        let fmt = ImageFmt {
            bits_per_pixel: pixmap_fmt.bits_per_pixel,
            scanline_pad: pixmap_fmt.scanline_pad,
            big_endian,
            red: Channel::new(visual.red_mask),
            green: Channel::new(visual.green_mask),
            blue: Channel::new(visual.blue_mask),
            fast_bgrx: pixmap_fmt.bits_per_pixel == 32
                && !big_endian
                && visual.red_mask == 0x00ff_0000
                && visual.green_mask == 0x0000_ff00
                && visual.blue_mask == 0x0000_00ff,
        };

        let mut cap = Capture {
            conn,
            root,
            width,
            height,
            fmt,
            damage: None,
            region: None,
            has_xfixes: false,
        };
        cap.setup_extensions();
        Ok(cap)
    }

    /// XDAMAGE and XFIXES are optional; without them we fall back to polling
    /// the whole screen.
    fn setup_extensions(&mut self) {
        let xfixes_ok = self
            .conn
            .xfixes_query_version(5, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some();
        self.has_xfixes = xfixes_ok;

        // Root configure events tell us when the screen is resized.
        let _ = self.conn.change_window_attributes(
            self.root,
            &xproto::ChangeWindowAttributesAux::new()
                .event_mask(xproto::EventMask::STRUCTURE_NOTIFY),
        );

        if xfixes_ok {
            let _ = self.conn.xfixes_select_cursor_input(
                self.root,
                xfixes::CursorNotifyMask::DISPLAY_CURSOR,
            );
        }

        let damage_ok = self
            .conn
            .damage_query_version(1, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some();

        if damage_ok && xfixes_ok {
            if let (Ok(dmg), Ok(reg)) = (self.conn.generate_id(), self.conn.generate_id()) {
                let created = self
                    .conn
                    .damage_create(dmg, self.root, damage::ReportLevel::NON_EMPTY)
                    .is_ok()
                    && self.conn.xfixes_create_region(reg, &[]).is_ok();
                if created && self.conn.flush().is_ok() {
                    self.damage = Some(dmg);
                    self.region = Some(reg);
                }
            }
        }
        let _ = self.conn.flush();
    }

    pub fn has_damage(&self) -> bool {
        self.damage.is_some()
    }

    /// Adopt a new screen size after a resize.
    pub fn set_size(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
    }

    /// Re-read the root window's real size, taking the X server's word for it
    /// after a grab raced a resize.
    pub fn resync_size(&mut self) -> XResult<(u16, u16)> {
        let geom = self.conn.get_geometry(self.root)?.reply()?;
        self.width = geom.width;
        self.height = geom.height;
        Ok((self.width, self.height))
    }

    /// Read one rectangle of the screen into `frame`. The rectangle is
    /// clipped to the screen, which can shrink under us when a client asks
    /// for a different resolution.
    pub fn grab(&self, frame: &mut Frame, r: Rect) -> XResult<Grab> {
        let r = Rect {
            x: r.x.min(self.width as u32),
            y: r.y.min(self.height as u32),
            w: r.w.min((self.width as u32).saturating_sub(r.x)),
            h: r.h.min((self.height as u32).saturating_sub(r.y)),
        };
        let r = Rect {
            w: r.w.min(frame.width.saturating_sub(r.x)),
            h: r.h.min(frame.height.saturating_sub(r.y)),
            ..r
        };
        if r.w == 0 || r.h == 0 {
            return Ok(Grab::Done);
        }
        let cookie = self.conn.get_image(
            ImageFormat::Z_PIXMAP,
            self.root,
            r.x as i16,
            r.y as i16,
            r.w as u16,
            r.h as u16,
            u32::MAX,
        )?;
        let reply = match cookie.reply() {
            Ok(reply) => reply,
            // The screen shrank between the clip above and this request, so
            // the rectangle is no longer inside the root. Recoverable: the
            // caller re-reads the size and starts over.
            Err(ReplyError::X11Error(e)) => {
                log::debug(&format!(
                    "GetImage refused ({:?}): the screen moved under us",
                    e.error_kind
                ));
                return Ok(Grab::Raced);
            }
            Err(e) => return Err(e.into()),
        };

        let bpp = self.fmt.bits_per_pixel as usize;
        let pad = self.fmt.scanline_pad as usize;
        let stride = ((r.w as usize * bpp + pad - 1) / pad) * (pad / 8);
        if reply.data.len() < stride * r.h as usize {
            // Same story, but the server answered before noticing. Never fatal
            // enough to be worth taking the whole desktop down for.
            log::warn("short GetImage reply; re-reading the screen size");
            return Ok(Grab::Raced);
        }

        for row in 0..r.h {
            let src = &reply.data[row as usize * stride..];
            let dst_start = (r.y + row) as usize * frame.width as usize + r.x as usize;
            let dst = &mut frame.px[dst_start..dst_start + r.w as usize];
            self.convert_row(src, dst);
        }
        Ok(Grab::Done)
    }

    #[inline]
    fn convert_row(&self, src: &[u8], dst: &mut [u32]) {
        let f = &self.fmt;
        if f.fast_bgrx {
            for (i, px) in dst.iter_mut().enumerate() {
                let b = &src[i * 4..i * 4 + 4];
                *px = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) & 0x00ff_ffff;
            }
            return;
        }
        let bytes = f.bits_per_pixel as usize / 8;
        for (i, px) in dst.iter_mut().enumerate() {
            let b = &src[i * bytes..i * bytes + bytes];
            let raw = match (bytes, f.big_endian) {
                (2, false) => u16::from_le_bytes([b[0], b[1]]) as u32,
                (2, true) => u16::from_be_bytes([b[0], b[1]]) as u32,
                (3, false) => u32::from_le_bytes([b[0], b[1], b[2], 0]),
                (3, true) => u32::from_be_bytes([0, b[0], b[1], b[2]]),
                (_, false) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                (_, true) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            };
            *px = f.red.take(raw) << 16 | f.green.take(raw) << 8 | f.blue.take(raw);
        }
    }

    /// Collect the damaged rectangles reported since the last call.
    fn take_damage(&self) -> XResult<Vec<Rect>> {
        let (Some(dmg), Some(reg)) = (self.damage, self.region) else {
            return Ok(Vec::new());
        };
        self.conn.damage_subtract(dmg, x11rb::NONE, reg)?;
        let reply = self.conn.xfixes_fetch_region(reg)?.reply()?;
        let mut out = Vec::with_capacity(reply.rectangles.len());
        for r in reply.rectangles {
            if let Some(rect) = self.clamp(r) {
                out.push(rect);
            }
        }
        Ok(out)
    }

    fn clamp(&self, r: xproto::Rectangle) -> Option<Rect> {
        let x0 = r.x.max(0) as u32;
        let y0 = r.y.max(0) as u32;
        let x1 = (r.x as i32 + r.width as i32).clamp(0, self.width as i32) as u32;
        let y1 = (r.y as i32 + r.height as i32).clamp(0, self.height as i32) as u32;
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some(Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
    }

    pub fn cursor(&self) -> XResult<Option<CursorImage>> {
        if !self.has_xfixes {
            return Ok(None);
        }
        let c = self.conn.xfixes_get_cursor_image()?.reply()?;
        if c.width == 0 || c.height == 0 {
            return Ok(None);
        }
        Ok(Some(CursorImage {
            width: c.width,
            height: c.height,
            hot_x: c.xhot,
            hot_y: c.yhot,
            argb: c.cursor_image,
        }))
    }

    fn full_rect(&self) -> Rect {
        Rect { x: 0, y: 0, w: self.width as u32, h: self.height as u32 }
    }
}

/// Capture loop: feeds the hub until it fails or the process exits.
pub fn run_capture(mut cap: Capture, hub: Arc<Hub>, max_fps: u32) -> XResult<()> {
    let interval = Duration::from_micros(1_000_000 / max_fps.clamp(1, 120) as u64);
    let idle = Duration::from_millis(250);
    let mut full = cap.full_rect();
    let mut idle_frames: u32 = 0;

    // Prime the framebuffer and the cursor.
    let mut stale = {
        let mut frame = hub.frame.write().unwrap();
        cap.grab(&mut frame, full)? == Grab::Raced
    };
    if let Ok(Some(c)) = cap.cursor() {
        hub.set_cursor(Some(Arc::new(c)));
    }

    loop {
        let started = Instant::now();

        let mut damaged = false;
        let mut cursor_changed = false;
        let mut resized = None;

        while let Some(event) = cap.conn.poll_for_event()? {
            match event {
                Event::DamageNotify(_) => damaged = true,
                Event::XfixesCursorNotify(_) => cursor_changed = true,
                Event::ConfigureNotify(e) if e.window == cap.root => {
                    if (e.width, e.height) != (cap.width, cap.height) {
                        resized = Some((e.width, e.height));
                    }
                }
                _ => {}
            }
        }

        // The screen changed size, either because a client asked or because
        // something else on the display did it.
        if let Some((w, h)) = resized {
            log::debug(&format!("capture: screen is now {w}x{h}"));
            cap.set_size(w, h);
            full = cap.full_rect();
            hub.resize(w as u32, h as u32, None);
            stale = true;
        }

        // The hub may already know about a resize we have not seen yet.
        let (hub_w, hub_h) = hub.dimensions();
        if (hub_w, hub_h) != (cap.width as u32, cap.height as u32) {
            cap.set_size(hub_w as u16, hub_h as u16);
            full = cap.full_rect();
            stale = true;
        }

        if !hub.has_clients() {
            // Nobody is watching: keep draining damage so the X server does
            // not accumulate it, but skip the expensive GetImage calls.
            if damaged {
                let _ = cap.take_damage()?;
                stale = true;
            }
            cap.conn.flush()?;
            std::thread::sleep(idle);
            continue;
        }

        let mut rects = if !cap.has_damage() {
            vec![full]
        } else if damaged {
            cap.take_damage()?
        } else {
            Vec::new()
        };

        if stale {
            rects = vec![full];
            stale = false;
        }

        let had_activity = !rects.is_empty() || cursor_changed || resized.is_some();
        if had_activity {
            idle_frames = 0;
        } else {
            idle_frames = idle_frames.saturating_add(1);
        }

        if !rects.is_empty() {
            log::debug(&format!("capture: {} rect(s), first {:?}", rects.len(), rects[0]));
            let mut raced = false;
            {
                let mut frame = hub.frame.write().unwrap();
                for r in &rects {
                    if cap.grab(&mut frame, *r)? == Grab::Raced {
                        raced = true;
                        break;
                    }
                }
            }
            if raced {
                // The screen changed size while we were reading it. Take the
                // server's word for the new size and redraw from scratch;
                // what we did read is a mix of two resolutions.
                let (w, h) = cap.resync_size()?;
                log::debug(&format!("capture: re-reading at {w}x{h} after a resize"));
                full = cap.full_rect();
                hub.resize(w as u32, h as u32, None);
                stale = true;
            } else {
                hub.damage(&rects);
            }
        }

        if cursor_changed {
            if let Ok(Some(c)) = cap.cursor() {
                hub.set_cursor(Some(Arc::new(c)));
            }
        }

        cap.conn.flush()?;
        let elapsed = started.elapsed();
        // Adaptive framing: burst to max_fps on activity, throttle gracefully when idle
        let target_interval = if idle_frames > 45 {
            Duration::from_millis(100)
        } else if idle_frames > 15 {
            Duration::from_millis(50)
        } else {
            interval
        };

        if elapsed < target_interval {
            std::thread::sleep(target_interval - elapsed);
        }
    }
}

/// Input side: a second X connection used only for XTEST.
pub struct Input {
    conn: RustConnection,
    root: xproto::Window,
    /// keysym -> (keycode, needs shift)
    map: HashMap<u32, (u8, bool)>,
    shift_keycode: Option<u8>,
    /// Keycodes with no symbols attached, reusable for unmapped keysyms.
    spare: Vec<u8>,
    spare_next: usize,
    /// keysym -> keycode we remapped it onto.
    remapped: HashMap<u32, u8>,
    remap_order: VecDeque<u32>,
    keysyms_per_keycode: u8,
    /// Keys the client currently holds down: keysym -> keycode.
    pressed: HashMap<u32, u8>,
    buttons: u32,
}

/// X11 keysyms we need by name.
mod keysym {
    pub const SHIFT_L: u32 = 0xffe1;
    pub const SHIFT_R: u32 = 0xffe2;
}

impl Input {
    pub fn open(display: Option<&str>) -> XResult<Self> {
        let (conn, screen_num) = RustConnection::connect(display)?;
        let root = conn.setup().roots[screen_num].root;
        conn.xtest_get_version(2, 2)?
            .reply()
            .map_err(|_| "the X server has no XTEST extension; input injection is impossible")?;

        let mut input = Input {
            conn,
            root,
            map: HashMap::new(),
            shift_keycode: None,
            spare: Vec::new(),
            spare_next: 0,
            remapped: HashMap::new(),
            remap_order: VecDeque::new(),
            keysyms_per_keycode: 1,
            pressed: HashMap::new(),
            buttons: 0,
        };
        input.load_keymap()?;
        Ok(input)
    }

    fn load_keymap(&mut self) -> XResult<()> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let count = max - min + 1;
        let reply = self.conn.get_keyboard_mapping(min, count)?.reply()?;
        let per = reply.keysyms_per_keycode.max(1);
        self.keysyms_per_keycode = per;

        self.map.clear();
        self.spare.clear();
        for i in 0..count as usize {
            let keycode = min + i as u8;
            let syms = &reply.keysyms[i * per as usize..(i + 1) * per as usize];
            if syms.iter().all(|&s| s == 0) {
                self.spare.push(keycode);
                continue;
            }
            // Column 0 is unshifted, column 1 is shifted; later columns need
            // group switching, which we do not drive.
            for (col, &sym) in syms.iter().take(2).enumerate() {
                if sym != 0 {
                    self.map.entry(sym).or_insert((keycode, col == 1));
                }
            }
        }

        let mods = self.conn.get_modifier_mapping()?.reply()?;
        let per_mod = mods.keycodes_per_modifier() as usize;
        self.shift_keycode = mods
            .keycodes
            .get(..per_mod)
            .and_then(|shift| shift.iter().copied().find(|&k| k != 0));
        Ok(())
    }

    fn fake(&self, type_: u8, detail: u8, x: i16, y: i16) -> XResult<()> {
        self.conn.xtest_fake_input(type_, detail, 0, self.root, x, y, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn pointer(&mut self, x: u16, y: u16, mask: u8) -> XResult<()> {
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x as i16, y as i16)?;
        // RFB button mask bits map 1:1 onto X button numbers 1..=8.
        for bit in 0..8u32 {
            let now = mask as u32 >> bit & 1;
            let before = self.buttons >> bit & 1;
            if now != before {
                let type_ = if now == 1 {
                    xproto::BUTTON_PRESS_EVENT
                } else {
                    xproto::BUTTON_RELEASE_EVENT
                };
                self.fake(type_, bit as u8 + 1, 0, 0)?;
            }
        }
        self.buttons = mask as u32;
        Ok(())
    }

    pub fn key(&mut self, keysym: u32, down: bool) -> XResult<()> {
        if !down {
            if let Some(keycode) = self.pressed.remove(&keysym) {
                self.fake(xproto::KEY_RELEASE_EVENT, keycode, 0, 0)?;
            }
            return Ok(());
        }
        if self.pressed.contains_key(&keysym) {
            // Auto-repeat: release and press again so the application sees it.
            let keycode = self.pressed[&keysym];
            self.fake(xproto::KEY_RELEASE_EVENT, keycode, 0, 0)?;
        }

        let (keycode, needs_shift) = match self.resolve(keysym)? {
            Some(v) => v,
            None => return Ok(()),
        };

        let client_holds_shift = self.pressed.contains_key(&keysym::SHIFT_L)
            || self.pressed.contains_key(&keysym::SHIFT_R);
        let synth_shift = needs_shift && !client_holds_shift;

        if synth_shift {
            if let Some(sk) = self.shift_keycode {
                self.fake(xproto::KEY_PRESS_EVENT, sk, 0, 0)?;
            }
        }
        self.fake(xproto::KEY_PRESS_EVENT, keycode, 0, 0)?;
        if synth_shift {
            if let Some(sk) = self.shift_keycode {
                self.fake(xproto::KEY_RELEASE_EVENT, sk, 0, 0)?;
            }
        }
        self.pressed.insert(keysym, keycode);
        Ok(())
    }

    /// Find a keycode for a keysym, borrowing a spare keycode for symbols the
    /// current layout cannot produce (the trick x11vnc uses).
    fn resolve(&mut self, keysym: u32) -> XResult<Option<(u8, bool)>> {
        if let Some(&v) = self.map.get(&keysym) {
            return Ok(Some(v));
        }
        if let Some(&kc) = self.remapped.get(&keysym) {
            return Ok(Some((kc, false)));
        }
        if self.spare.is_empty() {
            return Ok(None);
        }

        let kc = self.spare[self.spare_next % self.spare.len()];
        self.spare_next += 1;

        // Evict whoever was using this keycode before.
        if self.remap_order.len() >= self.spare.len() {
            if let Some(old) = self.remap_order.pop_front() {
                self.remapped.remove(&old);
            }
        }

        let syms = vec![keysym; self.keysyms_per_keycode as usize];
        self.conn
            .change_keyboard_mapping(1, kc, self.keysyms_per_keycode, &syms)?;
        self.conn.sync()?;
        // Give clients a moment to process the MappingNotify before we press.
        std::thread::sleep(Duration::from_millis(10));

        self.remapped.insert(keysym, kc);
        self.remap_order.push_back(keysym);
        Ok(Some((kc, false)))
    }

    /// Release everything this client left held down.
    pub fn release_all(&mut self) {
        let held: Vec<(u32, u8)> = self.pressed.drain().collect();
        for (_, keycode) in held {
            let _ = self.fake(xproto::KEY_RELEASE_EVENT, keycode, 0, 0);
        }
        for bit in 0..8u32 {
            if self.buttons >> bit & 1 == 1 {
                let _ = self.fake(xproto::BUTTON_RELEASE_EVENT, bit as u8 + 1, 0, 0);
            }
        }
        self.buttons = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_scales_to_eight_bits() {
        let c = Channel::new(0x00ff_0000);
        assert_eq!(c.take(0x00ab_0000), 0xab);
        let c565 = Channel::new(0xf800);
        assert_eq!(c565.take(0xf800), 255);
        assert_eq!(c565.take(0), 0);
    }
}

/// Screen resizing through RandR.
///
/// An X server can only shrink to sizes inside the range it reports, and
/// Xvfb's maximum is fixed at the size it was started with. rvnc therefore
/// starts Xvfb at the largest size it will ever need and shrinks from there.
pub struct Resizer {
    conn: RustConnection,
    root: xproto::Window,
    crtc: randr::Crtc,
    output: randr::Output,
    pub max: (u16, u16),
    pub min: (u16, u16),
}

impl Resizer {
    pub fn open(display: Option<&str>) -> Option<Self> {
        let (conn, screen_num) = RustConnection::connect(display).ok()?;
        let root = conn.setup().roots[screen_num].root;
        conn.randr_query_version(1, 2).ok()?.reply().ok()?;

        let range = conn.randr_get_screen_size_range(root).ok()?.reply().ok()?;
        let res = conn
            .randr_get_screen_resources_current(root)
            .ok()?
            .reply()
            .ok()?;

        Some(Resizer {
            conn,
            root,
            crtc: *res.crtcs.first()?,
            output: *res.outputs.first()?,
            max: (range.max_width, range.max_height),
            min: (range.min_width.max(16), range.min_height.max(16)),
        })
    }

    /// Resize the screen, clamped to what the server allows. Returns the size
    /// actually in effect afterwards.
    pub fn resize(&self, width: u16, height: u16) -> XResult<(u16, u16)> {
        let w = width.clamp(self.min.0, self.max.0);
        let h = height.clamp(self.min.1, self.max.1);

        let current = self.conn.get_geometry(self.root)?.reply()?;
        if (current.width, current.height) == (w, h) {
            return Ok((w, h));
        }

        // The CRTC has to let go of the framebuffer before it can shrink.
        let _ = self
            .conn
            .randr_set_crtc_config(
                self.crtc,
                CURRENT_TIME,
                CURRENT_TIME,
                0,
                0,
                0,
                randr::Rotation::ROTATE0,
                &[],
            )?
            .reply();

        // Millimetres at a nominal 96 dpi, so applications compute sane DPI.
        let mm = |px: u16| (px as u32 * 254 / 960).max(1);
        self.conn
            .randr_set_screen_size(self.root, w, h, mm(w), mm(h))?
            .check()?;

        // Re-attach the CRTC if the server happens to have a matching mode.
        // Xvfb does not let us add one, and leaving it off still gives the
        // right screen size, which is what clients and toolkits read.
        if let Ok(res) = self.conn.randr_get_screen_resources_current(self.root)?.reply() {
            if let Some(mode) = res.modes.iter().find(|m| m.width == w && m.height == h) {
                let _ = self.conn.randr_set_crtc_config(
                    self.crtc,
                    CURRENT_TIME,
                    CURRENT_TIME,
                    0,
                    0,
                    mode.id,
                    randr::Rotation::ROTATE0,
                    &[self.output],
                )?;
            }
        }

        self.conn.sync()?;
        let now = self.conn.get_geometry(self.root)?.reply()?;
        Ok((now.width, now.height))
    }
}
