//! Optional process supervision: start an Xvfb display and the desktop
//! session on it, so `rvnc xfce4-session` is all a user has to type.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
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

/// Directories that hold X servers but are not always on `PATH`, especially
/// in the trimmed-down containers rvnc tends to run in.
const EXTRA_BIN_DIRS: [&str; 5] = [
    "/usr/bin",
    "/usr/local/bin",
    "/usr/X11R6/bin",
    "/usr/libexec",
    "/opt/X11/bin",
];

/// Locate an executable, looking beyond `PATH`. An argument containing a
/// slash is taken as a path and used as is.
pub fn find_program(name: &str) -> Option<PathBuf> {
    let executable = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };

    if name.contains('/') {
        let path = PathBuf::from(name);
        return executable(&path).then_some(path);
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(name);
            if executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    EXTRA_BIN_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| executable(candidate))
}

/// rvnc serves an X display; it does not implement one. Creating a display
/// therefore needs a real X server on the machine.
fn no_x_server_error(requested: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "{requested} not found.\n\
             \n\
             rvnc is a VNC server, not an X server: to create a display it needs Xvfb.\n\
             Install it with one of:\n\
             \n    Alpine          apk add xvfb\
             \n    Debian/Ubuntu   apt install xvfb\
             \n    Fedora/RHEL     dnf install xorg-x11-server-Xvfb\
             \n    Arch            pacman -S xorg-server-xvfb\n\
             \n\
             Already have an X display running? Serve that one instead and no\n\
             X server is started:   rvnc --display :0"
        ),
    )
}

/// Start an X server on the first free display number and return its name
/// (`:7`). `program` must understand Xvfb's command line.
pub fn start_xvfb(
    sup: &Arc<Supervisor>,
    program: &str,
    width: u32,
    height: u32,
    depth: u32,
) -> io::Result<String> {
    let binary = find_program(program).ok_or_else(|| no_x_server_error(program))?;

    let number = free_display_number()
        .ok_or_else(|| io::Error::new(io::ErrorKind::AddrInUse, "no free X display number"))?;
    let display = format!(":{number}");

    let mut cmd = Command::new(&binary);
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

    let child = cmd
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("could not run {}: {e}", binary.display())))?;
    sup.adopt(child);

    wait_for_display(number, Duration::from_secs(10))?;
    log::info(&format!(
        "started {} on {display} ({width}x{height}x{depth})",
        binary.display()
    ));
    Ok(display)
}

/// Run the desktop session (or any command) against `display`.
pub fn start_session(
    sup: &Arc<Supervisor>,
    display: &str,
    argv: &[String],
) -> io::Result<()> {
    let binary = find_program(&argv[0]).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} not found. Install the desktop or program you want to run, \
                 or give rvnc a command that exists.",
                argv[0]
            ),
        )
    })?;

    let mut cmd = Command::new(&binary);
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
    fn finds_programs_on_path_and_rejects_missing_ones() {
        assert!(find_program("sh").is_some());
        assert!(find_program("definitely-not-a-real-binary-9271").is_none());
    }

    #[test]
    fn explicit_paths_are_used_as_given() {
        assert_eq!(find_program("/bin/sh").as_deref(), Some(Path::new("/bin/sh")));
        assert!(find_program("/bin/definitely-not-here-9271").is_none());
        // A directory is not a program.
        assert!(find_program("/usr/bin").is_none());
    }

    #[test]
    fn missing_x_server_explains_itself() {
        let err = no_x_server_error("Xvfb");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let text = err.to_string();
        assert!(text.contains("not an X server"), "{text}");
        assert!(text.contains("apk add xvfb"), "{text}");
        assert!(text.contains("--display"), "{text}");
    }

    #[test]
    fn start_xvfb_fails_clearly_when_no_x_server_exists() {
        let sup = Supervisor::new();
        let err = start_xvfb(&sup, "not-an-x-server-9271", 800, 600, 24).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("apt install xvfb"));
    }

    #[test]
    fn missing_session_command_says_so() {
        let sup = Supervisor::new();
        let err = start_session(&sup, ":9", &["no-such-desktop-9271".to_string()]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn stop_is_idempotent() {
        let sup = Supervisor::new();
        sup.stop();
        sup.stop();
    }
}
