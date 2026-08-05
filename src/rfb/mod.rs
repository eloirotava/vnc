//! RFB 3.8 server session (the VNC wire protocol).

pub mod auth;
pub mod zrle;

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use crate::pixel::{PixCodec, PixelFormat};
use crate::screen::{Caps, ClientLink, Hub, Rect};
use crate::x11::Input;
use zrle::ZrleEncoder;

const SEC_NONE: u8 = 1;
const SEC_VNC_AUTH: u8 = 2;

const MSG_SET_PIXEL_FORMAT: u8 = 0;
const MSG_SET_ENCODINGS: u8 = 2;
const MSG_FB_UPDATE_REQUEST: u8 = 3;
const MSG_KEY_EVENT: u8 = 4;
const MSG_POINTER_EVENT: u8 = 5;
const MSG_CLIENT_CUT_TEXT: u8 = 6;

const ENC_RAW: i32 = 0;
const ENC_ZRLE: i32 = 16;
const ENC_CURSOR: i32 = -239;
const ENC_LAST_RECT: i32 = -224;
const ENC_DESKTOP_SIZE: i32 = -223;

/// Guards against a client asking us to allocate unbounded memory.
const MAX_ENCODINGS: u16 = 4096;
const MAX_CUT_TEXT: u32 = 1 << 20;

pub struct Config {
    pub password: Option<String>,
    pub view_only: bool,
    pub desktop_name: String,
}

/// Run a full session. `reader` and `writer` are the two halves of one
/// connection; `writer` is shared because the transport may need to write
/// from the read side too (WebSocket pongs).
pub fn serve<R: Read + Send + 'static, W: Write + Send + 'static>(
    mut reader: R,
    writer: Arc<Mutex<W>>,
    hub: Arc<Hub>,
    input: Arc<Mutex<Input>>,
    cfg: Arc<Config>,
) -> io::Result<()> {
    let minor = handshake_version(&mut reader, &writer)?;
    authenticate(&mut reader, &writer, minor, &cfg)?;

    // ClientInit: one byte, the shared-desktop flag, which we always honour.
    let mut shared = [0u8; 1];
    reader.read_exact(&mut shared)?;

    let (width, height) = hub.dimensions();
    let mut init = Vec::with_capacity(32 + cfg.desktop_name.len());
    init.extend_from_slice(&(width as u16).to_be_bytes());
    init.extend_from_slice(&(height as u16).to_be_bytes());
    PixelFormat::default_rgb().write(&mut init);
    init.extend_from_slice(&(cfg.desktop_name.len() as u32).to_be_bytes());
    init.extend_from_slice(cfg.desktop_name.as_bytes());
    send(&writer, &init)?;

    let link = hub.register();
    let result = {
        let link_r = link.clone();
        let hub_r = hub.clone();
        let cfg_r = cfg.clone();
        let input_r = input.clone();
        let reader_thread = std::thread::spawn(move || {
            let r = read_loop(reader, &link_r, &hub_r, &input_r, &cfg_r);
            link_r.close();
            r
        });

        let write_result = write_loop(&writer, &link, &hub);
        link.close();
        let _ = reader_thread.join();
        write_result
    };

    hub.unregister(&link);
    if !cfg.view_only {
        input.lock().unwrap().release_all();
    }
    result
}

fn send<W: Write>(writer: &Arc<Mutex<W>>, buf: &[u8]) -> io::Result<()> {
    let mut w = writer.lock().unwrap();
    w.write_all(buf)?;
    w.flush()
}

fn handshake_version<R: Read, W: Write>(reader: &mut R, writer: &Arc<Mutex<W>>) -> io::Result<u8> {
    send(writer, b"RFB 003.008\n")?;
    let mut buf = [0u8; 12];
    reader.read_exact(&mut buf)?;
    if &buf[..4] != b"RFB " {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not an RFB client"));
    }
    let minor: u8 = std::str::from_utf8(&buf[8..11])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    Ok(minor)
}

fn authenticate<R: Read, W: Write>(
    reader: &mut R,
    writer: &Arc<Mutex<W>>,
    minor: u8,
    cfg: &Config,
) -> io::Result<()> {
    let want_auth = cfg.password.is_some();
    let sec_type = if want_auth { SEC_VNC_AUTH } else { SEC_NONE };

    if minor >= 7 {
        send(writer, &[1, sec_type])?;
        let mut chosen = [0u8; 1];
        reader.read_exact(&mut chosen)?;
        if chosen[0] != sec_type {
            let _ = fail_security(writer, minor, "unsupported security type");
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "client picked a security type we did not offer",
            ));
        }
    } else {
        // RFB 3.3: the server dictates the security type.
        send(writer, &(sec_type as u32).to_be_bytes())?;
    }

    if want_auth {
        let challenge = auth::challenge()?;
        send(writer, &challenge)?;
        let mut response = [0u8; 16];
        reader.read_exact(&mut response)?;
        let expected = auth::encrypt_challenge(cfg.password.as_ref().unwrap(), &challenge);
        if !auth::constant_time_eq(&response, &expected) {
            let _ = fail_security(writer, minor, "authentication failed");
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "authentication failed",
            ));
        }
    }

    // SecurityResult: always sent from 3.8 on, and after VNC auth in 3.7.
    if minor >= 8 || (minor == 7 && want_auth) || (minor < 7 && want_auth) {
        send(writer, &0u32.to_be_bytes())?;
    }
    Ok(())
}

fn fail_security<W: Write>(writer: &Arc<Mutex<W>>, minor: u8, reason: &str) -> io::Result<()> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&1u32.to_be_bytes());
    if minor >= 8 {
        buf.extend_from_slice(&(reason.len() as u32).to_be_bytes());
        buf.extend_from_slice(reason.as_bytes());
    }
    send(writer, &buf)
}

fn read_loop<R: Read>(
    mut reader: R,
    link: &Arc<ClientLink>,
    hub: &Arc<Hub>,
    input: &Arc<Mutex<Input>>,
    cfg: &Config,
) -> io::Result<()> {
    let mut type_ = [0u8; 1];
    loop {
        reader.read_exact(&mut type_)?;
        match type_[0] {
            MSG_SET_PIXEL_FORMAT => {
                let mut buf = [0u8; 19];
                reader.read_exact(&mut buf)?;
                let pf = PixelFormat::parse(&buf[3..]).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "unsupported pixel format")
                })?;
                let mut st = link.state.lock().unwrap();
                st.pf = pf;
                st.dirty.set_all();
                drop(st);
                link.wake();
            }
            MSG_SET_ENCODINGS => {
                let mut head = [0u8; 3];
                reader.read_exact(&mut head)?;
                let count = u16::from_be_bytes([head[1], head[2]]);
                if count > MAX_ENCODINGS {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "absurd encoding count",
                    ));
                }
                let mut caps = Caps::default();
                let mut buf = [0u8; 4];
                for _ in 0..count {
                    reader.read_exact(&mut buf)?;
                    match i32::from_be_bytes(buf) {
                        ENC_ZRLE => caps.zrle = true,
                        ENC_CURSOR => caps.cursor = true,
                        ENC_LAST_RECT => caps.last_rect = true,
                        ENC_DESKTOP_SIZE => caps.desktop_size = true,
                        _ => {}
                    }
                }
                let mut st = link.state.lock().unwrap();
                st.caps = caps;
                st.cursor_dirty = true;
                drop(st);
                link.wake();
            }
            MSG_FB_UPDATE_REQUEST => {
                let mut buf = [0u8; 9];
                reader.read_exact(&mut buf)?;
                let incremental = buf[0] != 0;
                let mut st = link.state.lock().unwrap();
                st.want_update = true;
                if !incremental {
                    st.want_full = true;
                }
                drop(st);
                link.wake();
            }
            MSG_KEY_EVENT => {
                let mut buf = [0u8; 7];
                reader.read_exact(&mut buf)?;
                if !cfg.view_only {
                    let down = buf[0] != 0;
                    let keysym = u32::from_be_bytes([buf[3], buf[4], buf[5], buf[6]]);
                    let _ = input.lock().unwrap().key(keysym, down);
                }
            }
            MSG_POINTER_EVENT => {
                let mut buf = [0u8; 5];
                reader.read_exact(&mut buf)?;
                if !cfg.view_only {
                    let mask = buf[0];
                    let x = u16::from_be_bytes([buf[1], buf[2]]);
                    let y = u16::from_be_bytes([buf[3], buf[4]]);
                    let (w, h) = hub.dimensions();
                    let x = x.min(w.saturating_sub(1) as u16);
                    let y = y.min(h.saturating_sub(1) as u16);
                    let _ = input.lock().unwrap().pointer(x, y, mask);
                }
            }
            MSG_CLIENT_CUT_TEXT => {
                let mut head = [0u8; 7];
                reader.read_exact(&mut head)?;
                let len = u32::from_be_bytes([head[3], head[4], head[5], head[6]]);
                if len > MAX_CUT_TEXT {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "oversized clipboard message",
                    ));
                }
                // Clipboard integration is not implemented yet; drain it.
                io::copy(&mut reader.by_ref().take(len as u64), &mut io::sink())?;
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown client message type {other}"),
                ));
            }
        }
    }
}

fn write_loop<W: Write>(
    writer: &Arc<Mutex<W>>,
    link: &Arc<ClientLink>,
    hub: &Arc<Hub>,
) -> io::Result<()> {
    let mut zrle = ZrleEncoder::new();
    let mut codec = PixCodec::new(PixelFormat::default_rgb());
    let mut msg = Vec::new();

    loop {
        let (pf, caps, rects, send_cursor) = {
            let mut st = link.state.lock().unwrap();
            loop {
                if st.closed {
                    return Ok(());
                }
                let has_work = st.want_full || !st.dirty.is_empty() || st.cursor_dirty;
                if st.want_update && has_work {
                    break;
                }
                st = link.cv.wait(st).unwrap();
            }
            let (w, h) = {
                // The frame cannot change size while a session is running, so
                // reading the dimensions from the tile map is enough.
                hub.dimensions()
            };
            let rects = if st.want_full {
                st.dirty.clear_all();
                vec![Rect { x: 0, y: 0, w, h }]
            } else {
                st.dirty.take_rects(w, h)
            };
            let send_cursor = st.cursor_dirty && st.caps.cursor;
            st.cursor_dirty = false;
            st.want_update = false;
            st.want_full = false;
            (st.pf, st.caps, rects, send_cursor)
        };

        if codec.format() != pf {
            codec = PixCodec::new(pf);
        }

        msg.clear();
        let count = rects.len() + send_cursor as usize;
        msg.push(0); // FramebufferUpdate
        msg.push(0);
        msg.extend_from_slice(&(count as u16).to_be_bytes());

        if send_cursor {
            write_cursor(&mut msg, hub, &codec);
        }

        if !rects.is_empty() {
            let frame = hub.frame.read().unwrap();
            for r in &rects {
                msg.extend_from_slice(&(r.x as u16).to_be_bytes());
                msg.extend_from_slice(&(r.y as u16).to_be_bytes());
                msg.extend_from_slice(&(r.w as u16).to_be_bytes());
                msg.extend_from_slice(&(r.h as u16).to_be_bytes());
                if caps.zrle {
                    msg.extend_from_slice(&ENC_ZRLE.to_be_bytes());
                    zrle.encode(&frame, &codec, *r, &mut msg);
                } else {
                    msg.extend_from_slice(&ENC_RAW.to_be_bytes());
                    for row in r.y..r.y + r.h {
                        codec.push_row(frame.row(row, r.x, r.w), &mut msg);
                    }
                }
            }
        }

        crate::http::log::debug(&format!(
            "update: {} rect(s), {} bytes", count, msg.len()));
        send(writer, &msg)?;
    }
}

/// Cursor pseudo-encoding: the client draws the pointer locally, so it stays
/// responsive regardless of round-trip time.
fn write_cursor(msg: &mut Vec<u8>, hub: &Hub, codec: &PixCodec) {
    let cursor = hub.cursor.lock().unwrap().clone();
    let Some(c) = cursor else {
        // An empty rectangle means "no cursor".
        msg.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        msg.extend_from_slice(&ENC_CURSOR.to_be_bytes());
        return;
    };

    msg.extend_from_slice(&c.hot_x.to_be_bytes());
    msg.extend_from_slice(&c.hot_y.to_be_bytes());
    msg.extend_from_slice(&c.width.to_be_bytes());
    msg.extend_from_slice(&c.height.to_be_bytes());
    msg.extend_from_slice(&ENC_CURSOR.to_be_bytes());

    for &argb in &c.argb {
        let a = argb >> 24 & 0xff;
        // XFIXES hands out premultiplied alpha; undo it so partly transparent
        // pixels do not come out dark once the mask makes them opaque.
        let un = |c: u32| if a == 0 { 0 } else { (c * 255 / a).min(255) };
        let (r, g, b) = (un(argb >> 16 & 0xff), un(argb >> 8 & 0xff), un(argb & 0xff));
        codec.push_pixel(r << 16 | g << 8 | b, msg);
    }

    let stride = (c.width as usize).div_ceil(8);
    for y in 0..c.height as usize {
        for byte in 0..stride {
            let mut bits = 0u8;
            for bit in 0..8 {
                let x = byte * 8 + bit;
                if x >= c.width as usize {
                    break;
                }
                let a = c.argb[y * c.width as usize + x] >> 24 & 0xff;
                if a >= 128 {
                    bits |= 0x80 >> bit;
                }
            }
            msg.push(bits);
        }
    }
}
