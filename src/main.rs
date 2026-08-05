//! rvnc — a single-binary VNC server for an X display, with the noVNC web
//! client built in. Point a browser at the port and you get the desktop.

mod http;
mod pixel;
mod rfb;
mod screen;
mod spawn;
mod ws;
mod x11;

use std::io::Read;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use http::log;
use rfb::Config;
use screen::Hub;
use spawn::Supervisor;

const USAGE: &str = "\
rvnc — serve an X display to any browser over VNC

USAGE:
    rvnc [OPTIONS] [--] [COMMAND [ARGS...]]

    rvnc xfce4-session          start a virtual display, run the desktop, serve it
    rvnc --display :1           serve a display that is already running

OPTIONS:
    -l, --listen ADDR       where to serve, PORT or HOST:PORT [default: 0.0.0.0:6080]
    -d, --display NAME      use an existing X display instead of starting Xvfb
    -g, --geometry WxH      size of the display rvnc starts [default: 1440x900]
        --depth N           colour depth of the display rvnc starts [default: 24]
    -p, --password PASS     VNC password (max 8 characters, as the protocol allows)
        --password-file F   read the password from a file (first line)
        --no-password       serve with no authentication at all
        --view-only         ignore keyboard and pointer input from clients
        --max-fps N         screen polling limit [default: 30]
    -v, --verbose           log more
    -h, --help              show this help
    -V, --version           show the version

If no password is given, rvnc generates one and prints it at startup.
";

struct Args {
    listen: String,
    display: Option<String>,
    geometry: (u32, u32),
    depth: u32,
    password: Option<String>,
    no_password: bool,
    view_only: bool,
    max_fps: u32,
    verbose: bool,
    command: Vec<String>,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            listen: "0.0.0.0:6080".into(),
            display: None,
            geometry: (1440, 900),
            depth: 24,
            password: None,
            no_password: false,
            view_only: false,
            max_fps: 30,
            verbose: false,
            command: Vec::new(),
        }
    }
}

type Fail = Box<dyn std::error::Error + Send + Sync>;

fn main() {
    match run() {
        Ok(()) => {}
        Err(e) => {
            eprintln!("rvnc: error: {e}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), Fail> {
    let args = match parse_args(std::env::args().skip(1))? {
        Some(args) => args,
        None => return Ok(()),
    };
    log::set_verbose(args.verbose);

    let sup = Supervisor::new();
    spawn::install_signal_handlers(sup.clone());

    // Work out which display to serve, starting one if we have to.
    let display = match (&args.display, args.command.is_empty()) {
        (Some(d), _) => d.clone(),
        (None, false) => spawn::start_xvfb(&sup, args.geometry.0, args.geometry.1, args.depth)?,
        (None, true) => std::env::var("DISPLAY").map_err(|_| {
            "no display to serve: pass --display, set DISPLAY, or give a command to run"
        })?,
    };

    if !args.command.is_empty() {
        spawn::start_session(&sup, &display, &args.command)?;
        spawn::watch_session(sup.clone());
    }

    let capture = x11::Capture::open(Some(&display))
        .map_err(|e| format!("cannot open X display {display}: {e}"))?;
    if !capture.has_damage() {
        log::warn("XDAMAGE unavailable; falling back to full-screen polling");
    }
    let (width, height) = (capture.width as u32, capture.height as u32);

    let input = x11::Input::open(Some(&display))
        .map_err(|e| format!("cannot set up input injection on {display}: {e}"))?;

    let hub = Hub::new(width, height);
    let password = resolve_password(&args)?;

    let cfg = Arc::new(Config {
        password: password.clone(),
        view_only: args.view_only,
        desktop_name: format!("rvnc {display}"),
    });

    let listener = TcpListener::bind(bind_addr(&args.listen)?)
        .map_err(|e| format!("cannot bind {}: {e}", args.listen))?;
    let local = listener.local_addr()?;

    {
        let hub = hub.clone();
        let max_fps = args.max_fps;
        std::thread::Builder::new()
            .name("capture".into())
            .spawn(move || {
                if let Err(e) = x11::run_capture(capture, hub, max_fps) {
                    log::warn(&format!("capture stopped: {e}"));
                    std::process::exit(1);
                }
            })?;
    }

    let host = if local.ip().is_unspecified() {
        "localhost".to_string()
    } else {
        local.ip().to_string()
    };
    log::info(&format!("serving X display {display} ({width}x{height})"));
    match &password {
        Some(p) => {
            // A link that lands straight on the desktop, plus the bare one for
            // anyone who would rather type the password into the prompt.
            log::info(&format!("open http://{host}:{}/?password={p}", local.port()));
            log::info(&format!("  or http://{host}:{}/ and enter: {p}", local.port()));
        }
        None => {
            log::info(&format!("open http://{host}:{}/", local.port()));
            log::warn("no password: anyone who can reach this port has the desktop");
        }
    }
    if args.view_only {
        log::info("view-only: client input is ignored");
    }

    let server = Arc::new(http::Server {
        hub,
        input: Arc::new(Mutex::new(input)),
        cfg,
    });
    http::run(listener, server)?;
    sup.stop();
    Ok(())
}

fn resolve_password(args: &Args) -> Result<Option<String>, Fail> {
    if args.no_password {
        return Ok(None);
    }
    if let Some(p) = &args.password {
        if p.len() > 8 {
            log::warn("VNC passwords are limited to 8 characters; the rest is ignored");
        }
        return Ok(Some(p.clone()));
    }
    Ok(Some(random_password()?))
}

/// Eight characters from an unambiguous alphabet, straight from the OS.
fn random_password() -> Result<String, Fail> {
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut bytes = [0u8; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect())
}

fn bind_addr(spec: &str) -> Result<String, Fail> {
    if let Ok(port) = spec.parse::<u16>() {
        return Ok(format!("0.0.0.0:{port}"));
    }
    if let Some(port) = spec.strip_prefix(':') {
        let port: u16 = port.parse().map_err(|_| format!("bad port in {spec:?}"))?;
        return Ok(format!("0.0.0.0:{port}"));
    }
    if !spec.contains(':') {
        return Err(format!("--listen needs a port, got {spec:?}").into());
    }
    Ok(spec.to_string())
}

fn parse_geometry(spec: &str) -> Result<(u32, u32), Fail> {
    let (w, h) = spec
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("bad geometry {spec:?}, expected WIDTHxHEIGHT"))?;
    let w: u32 = w.trim().parse().map_err(|_| format!("bad width in {spec:?}"))?;
    let h: u32 = h.trim().parse().map_err(|_| format!("bad height in {spec:?}"))?;
    if !(16..=16384).contains(&w) || !(16..=16384).contains(&h) {
        return Err(format!("geometry {spec:?} is out of range").into());
    }
    Ok((w, h))
}

/// Returns `None` when the program should exit after printing help.
fn parse_args<I: Iterator<Item = String>>(args: I) -> Result<Option<Args>, Fail> {
    let mut out = Args::default();
    let mut args = args.peekable();
    let need = |v: Option<String>, flag: &str| -> Result<String, Fail> {
        v.ok_or_else(|| format!("{flag} needs a value").into())
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("rvnc {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-l" | "--listen" => out.listen = need(args.next(), "--listen")?,
            "-d" | "--display" => out.display = Some(need(args.next(), "--display")?),
            "-g" | "--geometry" => out.geometry = parse_geometry(&need(args.next(), "--geometry")?)?,
            "--depth" => {
                let d: u32 = need(args.next(), "--depth")?.parse()?;
                if !matches!(d, 16 | 24 | 30) {
                    return Err(format!("unsupported depth {d}, use 16, 24 or 30").into());
                }
                out.depth = d;
            }
            "-p" | "--password" => out.password = Some(need(args.next(), "--password")?),
            "--password-file" => {
                let path = need(args.next(), "--password-file")?;
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("cannot read {path}: {e}"))?;
                let line = text.lines().next().unwrap_or("").trim().to_string();
                if line.is_empty() {
                    return Err(format!("{path} is empty").into());
                }
                out.password = Some(line);
            }
            "--no-password" => out.no_password = true,
            "--view-only" => out.view_only = true,
            "--max-fps" => {
                let n: u32 = need(args.next(), "--max-fps")?.parse()?;
                out.max_fps = n.clamp(1, 120);
            }
            "-v" | "--verbose" => out.verbose = true,
            "--" => {
                out.command.extend(args.by_ref());
                break;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unknown option {other:?} (try --help)").into());
            }
            other => {
                // First bare word starts the command to run.
                out.command.push(other.to_string());
                out.command.extend(args.by_ref());
                break;
            }
        }
    }

    if out.password.is_some() && out.no_password {
        return Err("--password and --no-password contradict each other".into());
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        parse_args(args.iter().map(|s| s.to_string())).unwrap().unwrap()
    }

    #[test]
    fn defaults_match_the_documented_ones() {
        let a = parse(&[]);
        assert_eq!(a.listen, "0.0.0.0:6080");
        assert_eq!(a.geometry, (1440, 900));
        assert!(a.command.is_empty());
        assert!(!a.view_only);
    }

    #[test]
    fn bare_command_is_captured_with_its_own_flags() {
        let a = parse(&["--display", ":3", "xfce4-session", "--replace", "-v"]);
        assert_eq!(a.display.as_deref(), Some(":3"));
        assert_eq!(a.command, vec!["xfce4-session", "--replace", "-v"]);
        assert!(!a.verbose, "flags after the command belong to the command");
    }

    #[test]
    fn double_dash_separates_the_command() {
        let a = parse(&["-v", "--", "--weird-binary-name"]);
        assert!(a.verbose);
        assert_eq!(a.command, vec!["--weird-binary-name"]);
    }

    #[test]
    fn listen_accepts_port_only() {
        assert_eq!(bind_addr("6080").unwrap(), "0.0.0.0:6080");
        assert_eq!(bind_addr(":9000").unwrap(), "0.0.0.0:9000");
        assert_eq!(bind_addr("127.0.0.1:6080").unwrap(), "127.0.0.1:6080");
        assert!(bind_addr("localhost").is_err());
    }

    #[test]
    fn geometry_parsing() {
        assert_eq!(parse_geometry("1920x1080").unwrap(), (1920, 1080));
        assert_eq!(parse_geometry("800X600").unwrap(), (800, 600));
        assert!(parse_geometry("1920").is_err());
        assert!(parse_geometry("1x1").is_err());
    }

    #[test]
    fn unknown_options_are_rejected() {
        assert!(parse_args(["--nope".to_string()].into_iter()).is_err());
    }

    #[test]
    fn contradicting_password_flags_are_rejected() {
        let r = parse_args(
            ["--password", "x", "--no-password"].iter().map(|s| s.to_string()),
        );
        assert!(r.is_err());
    }

    #[test]
    fn generated_passwords_are_eight_usable_characters() {
        let p = random_password().unwrap();
        assert_eq!(p.len(), 8);
        assert!(p.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(p, random_password().unwrap());
    }
}
