//! Shared framebuffer plus per-client dirty tracking.
//!
//! The capture thread owns writes to the [`Frame`]; every connected client
//! gets a [`ClientLink`] holding the tiles it still has to send. Damage is
//! tracked at 64x64 tile granularity, which matches ZRLE's tile size.

use std::sync::{Arc, Condvar, Mutex, RwLock};

use crate::pixel::PixelFormat;

pub const TILE: u32 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// The captured screen, one `u32` per pixel in `0x00RRGGBB`.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub px: Vec<u32>,
}

impl Frame {
    pub fn new(width: u32, height: u32) -> Self {
        Frame { width, height, px: vec![0; (width as usize) * (height as usize)] }
    }

    #[inline]
    pub fn row(&self, y: u32, x: u32, w: u32) -> &[u32] {
        let start = (y as usize) * (self.width as usize) + x as usize;
        &self.px[start..start + w as usize]
    }
}

/// A cursor shape from XFIXES, in premultiplied ARGB.
pub struct CursorImage {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub argb: Vec<u32>,
}

/// Bitmap of dirty 64x64 tiles.
pub struct TileMap {
    cols: u32,
    rows: u32,
    bits: Vec<u64>,
    set_count: usize,
}

impl TileMap {
    pub fn new(width: u32, height: u32) -> Self {
        let cols = width.div_ceil(TILE);
        let rows = height.div_ceil(TILE);
        let words = ((cols as usize) * (rows as usize)).div_ceil(64);
        TileMap { cols, rows, bits: vec![0; words], set_count: 0 }
    }

    #[inline]
    fn idx(&self, col: u32, row: u32) -> usize {
        (row as usize) * (self.cols as usize) + col as usize
    }

    #[inline]
    fn get(&self, col: u32, row: u32) -> bool {
        let i = self.idx(col, row);
        self.bits[i / 64] >> (i % 64) & 1 == 1
    }

    #[inline]
    fn set(&mut self, col: u32, row: u32) {
        let i = self.idx(col, row);
        let w = &mut self.bits[i / 64];
        if *w >> (i % 64) & 1 == 0 {
            *w |= 1 << (i % 64);
            self.set_count += 1;
        }
    }

    #[inline]
    fn clear(&mut self, col: u32, row: u32) {
        let i = self.idx(col, row);
        let w = &mut self.bits[i / 64];
        if *w >> (i % 64) & 1 == 1 {
            *w &= !(1 << (i % 64));
            self.set_count -= 1;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.set_count == 0
    }

    pub fn set_all(&mut self) {
        for r in 0..self.rows {
            for c in 0..self.cols {
                self.set(c, r);
            }
        }
    }

    pub fn clear_all(&mut self) {
        for w in &mut self.bits {
            *w = 0;
        }
        self.set_count = 0;
    }

    /// Mark every tile overlapping the given pixel rectangle.
    pub fn add_rect(&mut self, r: Rect) {
        if r.w == 0 || r.h == 0 {
            return;
        }
        let c0 = r.x / TILE;
        let r0 = r.y / TILE;
        let c1 = ((r.x + r.w - 1) / TILE).min(self.cols.saturating_sub(1));
        let r1 = ((r.y + r.h - 1) / TILE).min(self.rows.saturating_sub(1));
        for row in r0..=r1 {
            for col in c0..=c1 {
                if col < self.cols && row < self.rows {
                    self.set(col, row);
                }
            }
        }
    }

    /// Drain the dirty tiles into pixel rectangles, greedily merging runs
    /// horizontally and then vertically. Clamped to `width`/`height` so edge
    /// tiles do not overhang the screen.
    pub fn take_rects(&mut self, width: u32, height: u32) -> Vec<Rect> {
        let mut out = Vec::new();
        for row in 0..self.rows {
            let mut col = 0;
            while col < self.cols {
                if !self.get(col, row) {
                    col += 1;
                    continue;
                }
                let start = col;
                while col < self.cols && self.get(col, row) {
                    col += 1;
                }
                let end = col; // exclusive

                // Extend downwards while the whole run stays dirty.
                let mut last_row = row;
                'grow: for r in (row + 1)..self.rows {
                    for c in start..end {
                        if !self.get(c, r) {
                            break 'grow;
                        }
                    }
                    last_row = r;
                }
                for r in row..=last_row {
                    for c in start..end {
                        self.clear(c, r);
                    }
                }

                let x = start * TILE;
                let y = row * TILE;
                let w = (end * TILE).min(width) - x;
                let h = ((last_row + 1) * TILE).min(height) - y;
                out.push(Rect { x, y, w, h });
            }
        }
        out
    }
}

/// Everything a session thread waits on, behind one mutex so a single
/// condvar can cover damage, cursor changes, client requests and shutdown.
pub struct LinkState {
    pub dirty: TileMap,
    pub cursor_dirty: bool,
    pub closed: bool,
    /// Set by the client's `FramebufferUpdateRequest`.
    pub want_update: bool,
    /// The pending request was non-incremental: resend everything.
    pub want_full: bool,
    pub pf: PixelFormat,
    pub caps: Caps,
    /// Set when the screen changed size: the value is the RFB "reason" code
    /// (0 server-initiated, 1 this client asked, 2 another client asked).
    pub pending_resize: Option<u16>,
    /// Clipboard text waiting to go out, already in Latin-1.
    pub clipboard: Option<Arc<Vec<u8>>>,
}

/// Which encodings the client asked for.
#[derive(Clone, Copy, Debug)]
#[derive(Default)]
pub struct Caps {
    pub zrle: bool,
    pub cursor: bool,
    pub last_rect: bool,
    pub desktop_size: bool,
    pub ext_desktop_size: bool,
}

pub struct ClientLink {
    pub state: Mutex<LinkState>,
    pub cv: Condvar,
}

impl ClientLink {
    pub fn wake(&self) {
        self.cv.notify_all();
    }

    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.cv.notify_all();
    }
}

/// Central hand-off point between the X capture thread and client sessions.
pub struct Hub {
    pub frame: RwLock<Frame>,
    pub cursor: Mutex<Option<Arc<CursorImage>>>,
    /// The most recent clipboard text, handed to clients as they connect.
    clipboard: Mutex<Option<Arc<Vec<u8>>>>,
    clients: Mutex<Vec<Arc<ClientLink>>>,
    /// Lets the capture loop skip work when nobody is connected.
    client_count: Mutex<usize>,
}

impl Hub {
    pub fn new(width: u32, height: u32) -> Arc<Self> {
        Arc::new(Hub {
            frame: RwLock::new(Frame::new(width, height)),
            cursor: Mutex::new(None),
            clipboard: Mutex::new(None),
            clients: Mutex::new(Vec::new()),
            client_count: Mutex::new(0),
        })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        let f = self.frame.read().unwrap();
        (f.width, f.height)
    }

    pub fn has_clients(&self) -> bool {
        *self.client_count.lock().unwrap() > 0
    }

    pub fn register(&self) -> Arc<ClientLink> {
        let (w, h) = self.dimensions();
        let mut dirty = TileMap::new(w, h);
        dirty.set_all();
        let link = Arc::new(ClientLink {
            state: Mutex::new(LinkState {
                dirty,
                cursor_dirty: true,
                closed: false,
                want_update: false,
                want_full: false,
                pf: PixelFormat::default_rgb(),
                caps: Caps::default(),
                pending_resize: None,
                clipboard: self.clipboard.lock().unwrap().clone(),
            }),
            cv: Condvar::new(),
        });
        self.clients.lock().unwrap().push(link.clone());
        *self.client_count.lock().unwrap() += 1;
        link
    }

    pub fn unregister(&self, link: &Arc<ClientLink>) {
        let mut clients = self.clients.lock().unwrap();
        let before = clients.len();
        clients.retain(|c| !Arc::ptr_eq(c, link));
        let removed = before - clients.len();
        drop(clients);
        let mut n = self.client_count.lock().unwrap();
        *n = n.saturating_sub(removed);
    }

    /// Report changed screen regions to every client.
    pub fn damage(&self, rects: &[Rect]) {
        if rects.is_empty() {
            return;
        }
        for c in self.clients.lock().unwrap().iter() {
            let mut st = c.state.lock().unwrap();
            for r in rects {
                st.dirty.add_rect(*r);
            }
            drop(st);
            c.cv.notify_all();
        }
    }

    /// Adopt a new screen size: the framebuffer is replaced and every client
    /// is told to redraw everything.
    pub fn resize(&self, width: u32, height: u32, requester: Option<&Arc<ClientLink>>) {
        {
            let mut frame = self.frame.write().unwrap();
            if frame.width == width && frame.height == height {
                return;
            }
            *frame = Frame::new(width, height);
        }
        for c in self.clients.lock().unwrap().iter() {
            let mut st = c.state.lock().unwrap();
            st.dirty = TileMap::new(width, height);
            st.dirty.set_all();
            st.cursor_dirty = true;
            st.pending_resize = Some(match requester {
                Some(r) if Arc::ptr_eq(r, c) => 1,
                Some(_) => 2,
                None => 0,
            });
            drop(st);
            c.cv.notify_all();
        }
    }

    /// Send clipboard text to every client except `origin`, which is where it
    /// came from.
    pub fn broadcast_clipboard(&self, text: Arc<Vec<u8>>, origin: Option<&Arc<ClientLink>>) {
        *self.clipboard.lock().unwrap() = Some(text.clone());
        for c in self.clients.lock().unwrap().iter() {
            if origin.map(|o| Arc::ptr_eq(o, c)).unwrap_or(false) {
                continue;
            }
            let mut st = c.state.lock().unwrap();
            st.clipboard = Some(text.clone());
            drop(st);
            c.cv.notify_all();
        }
    }

    pub fn set_cursor(&self, img: Option<Arc<CursorImage>>) {
        *self.cursor.lock().unwrap() = img;
        for c in self.clients.lock().unwrap().iter() {
            c.state.lock().unwrap().cursor_dirty = true;
            c.cv.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_cover_rect() {
        let mut t = TileMap::new(200, 200);
        t.add_rect(Rect { x: 10, y: 10, w: 1, h: 1 });
        assert!(!t.is_empty());
        let rects = t.take_rects(200, 200);
        assert_eq!(rects, vec![Rect { x: 0, y: 0, w: 64, h: 64 }]);
        assert!(t.is_empty());
    }

    #[test]
    fn full_screen_merges_into_one_rect() {
        let mut t = TileMap::new(640, 480);
        t.set_all();
        let rects = t.take_rects(640, 480);
        assert_eq!(rects, vec![Rect { x: 0, y: 0, w: 640, h: 480 }]);
    }

    #[test]
    fn edge_tiles_are_clamped() {
        let mut t = TileMap::new(100, 100);
        t.add_rect(Rect { x: 90, y: 90, w: 10, h: 10 });
        let rects = t.take_rects(100, 100);
        assert_eq!(rects, vec![Rect { x: 64, y: 64, w: 36, h: 36 }]);
    }

    #[test]
    fn separate_regions_stay_separate() {
        let mut t = TileMap::new(640, 480);
        t.add_rect(Rect { x: 0, y: 0, w: 10, h: 10 });
        t.add_rect(Rect { x: 500, y: 400, w: 10, h: 10 });
        let rects = t.take_rects(640, 480);
        assert_eq!(rects.len(), 2);
        assert!(t.is_empty());
    }

    #[test]
    fn resize_replaces_the_frame_and_marks_everyone() {
        let hub = Hub::new(320, 240);
        let a = hub.register();
        let b = hub.register();
        for link in [&a, &b] {
            let mut st = link.state.lock().unwrap();
            st.dirty.clear_all();
            st.pending_resize = None;
        }

        hub.resize(640, 480, Some(&a));
        assert_eq!(hub.dimensions(), (640, 480));
        // The client that asked is told so; the other sees a foreign resize.
        assert_eq!(a.state.lock().unwrap().pending_resize, Some(1));
        assert_eq!(b.state.lock().unwrap().pending_resize, Some(2));
        assert!(!a.state.lock().unwrap().dirty.is_empty());

        // A no-op resize does not disturb anyone.
        b.state.lock().unwrap().pending_resize = None;
        hub.resize(640, 480, None);
        assert_eq!(b.state.lock().unwrap().pending_resize, None);
    }

    #[test]
    fn clipboard_skips_the_client_it_came_from() {
        let hub = Hub::new(320, 240);
        let a = hub.register();
        let b = hub.register();
        hub.broadcast_clipboard(Arc::new(b"hi".to_vec()), Some(&a));
        assert!(a.state.lock().unwrap().clipboard.is_none());
        assert_eq!(b.state.lock().unwrap().clipboard.as_deref(), Some(&b"hi".to_vec()));

        // Someone joining later still receives what is on the clipboard.
        let c = hub.register();
        assert_eq!(c.state.lock().unwrap().clipboard.as_deref(), Some(&b"hi".to_vec()));
    }

    #[test]
    fn damage_reaches_registered_clients() {
        let hub = Hub::new(320, 240);
        let link = hub.register();
        link.state.lock().unwrap().dirty.clear_all();
        hub.damage(&[Rect { x: 0, y: 0, w: 8, h: 8 }]);
        assert!(!link.state.lock().unwrap().dirty.is_empty());
        hub.unregister(&link);
        assert!(!hub.has_clients());
    }
}
