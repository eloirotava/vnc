//! Optional process supervision: start an Xvfb display and the desktop
//! session on it, so `rvnc xfce4-session` is all a user has to type.

use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::http::log;

/// Children we must take down before exiting.
#[derive(Default)]
pub struct Supervisor {
    children: Mutex<Vec<Child>>,
    stopping: AtomicBool,
}

impl Supervisor {
    pub fn new() -> Arc<Self> {
        Arc::new(Supervisor::default())
    }

    fn adopt(&self, child: Child) {
        self.children.lock().unwrap().push(child);
    }

    /// Kill everything we started. Safe to call more than once.
    pub fn stop(&self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut children = self.children.lock().unwrap();
        // Reverse order: the session first, then the X server under it.
        for child in children.iter_mut().rev() {
            let _ = child.kill();
        }
        for child in children.iter_mut() {
            let _ = child.wait();
        }
        children.clear();
    }
}

/// Start `Xvfb` on the first free display number and return its name (`:7`).
pub fn start_xvfb(
    sup: &Arc<Supervisor>,
    width: u32,
    height: u32,
    depth: u32,
) -> io::Result<String> {
    let number = free_display_number()
        .ok_or_else(|| io::Error::new(io::ErrorKind::AddrInUse, "no free X display number"))?;
    let display = format!(":{number}");

    let mut cmd = Command::new("Xvfb");
    cmd.arg(&display)
        .arg("-screen")
        .arg("0")
        .arg(format!("{width}x{height}x{depth}"))
        .arg("-nolisten")
        .arg("tcp")
        .arg("-noreset")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let child = cmd.spawn().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("could not start Xvfb ({e}); install it or pass --display to use a running X server"),
        )
    })?;
    sup.adopt(child);

    wait_for_display(number, Duration::from_secs(10))?;
    log::info(&format!("started Xvfb on {display} ({width}x{height}x{depth})"));
    Ok(display)
}

/// Run the desktop session (or any command) against `display`.
pub fn start_session(
    sup: &Arc<Supervisor>,
    display: &str,
    argv: &[String],
) -> io::Result<()> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env("DISPLAY", display)
        .stdin(Stdio::null());
    let child = cmd.spawn().map_err(|e| {
        io::Error::new(e.kind(), format!("could not run {:?}: {e}", argv[0]))
    })?;
    log::info(&format!("running {} on {display}", argv.join(" ")));
    sup.adopt(child);
    Ok(())
}

/// Watch the session process; when it exits, bring the server down with it.
pub fn watch_session(sup: Arc<Supervisor>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(250));
        let mut done = None;
        {
            let mut children = sup.children.lock().unwrap();
            if let Some(session) = children.last_mut() {
                if let Ok(Some(status)) = session.try_wait() {
                    done = Some(status);
                }
            }
        }
        if let Some(status) = done {
            log::info(&format!("session exited ({status}); shutting down"));
            sup.stop();
            std::process::exit(0);
        }
    });
}

fn display_socket(number: u32) -> String {
    format!("/tmp/.X11-unix/X{number}")
}

fn free_display_number() -> Option<u32> {
    (1..100).find(|&n| {
        !Path::new(&display_socket(n)).exists() && !Path::new(&format!("/tmp/.X{n}-lock")).exists()
    })
}

fn wait_for_display(number: u32, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let socket = display_socket(number);
    while Instant::now() < deadline {
        if Path::new(&socket).exists() {
            // The socket exists slightly before the server accepts clients.
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("X display :{number} did not come up"),
    ))
}

/// Install handlers so Ctrl-C and `kill` take the children down too.
pub fn install_signal_handlers(sup: Arc<Supervisor>) {
    static SIGNALLED: AtomicBool = AtomicBool::new(false);

    extern "C" fn handler(_: libc::c_int) {
        SIGNALLED.store(true, Ordering::SeqCst);
    }

    // SAFETY: the handler only performs an atomic store, which is
    // async-signal-safe; the actual shutdown happens on the watcher thread.
    unsafe {
        let handler = handler as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGHUP, handler);
        // Do not die when a browser drops a connection mid-write.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    std::thread::spawn(move || loop {
        if SIGNALLED.load(Ordering::SeqCst) {
            log::info("shutting down");
            sup.stop();
            std::process::exit(0);
        }
        std::thread::sleep(Duration::from_millis(100));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_numbers_skip_existing_sockets() {
        // Whatever it returns must be genuinely unused.
        if let Some(n) = free_display_number() {
            assert!(!Path::new(&display_socket(n)).exists());
        }
    }

    #[test]
    fn waiting_for_a_dead_display_times_out() {
        let err = wait_for_display(98, Duration::from_millis(120)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn stop_is_idempotent() {
        let sup = Supervisor::new();
        sup.stop();
        sup.stop();
    }
}
