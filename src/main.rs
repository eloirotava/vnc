//! rvnc — a single-binary VNC server for an X display, with the noVNC web
//! client built in. Point a browser at the port and you get the desktop.

mod clipboard;
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

    rvnc xfce4-session          start Xvfb, run the desktop on it, serve it
    rvnc --display :1           serve a display that is already running

rvnc serves an X display, it is not an X server itself. Given a command it
starts Xvfb (which must be installed) and runs the command there; with
--display it attaches to a display you already have and starts nothing.

OPTIONS:
    -l, --listen ADDR       where to serve, PORT or HOST:PORT [default: 0.0.0.0:6080]
    -d, --display NAME      use an existing X display instead of starting Xvfb
    -g, --geometry WxH      starting size of the display [default: 1440x900]
        --max-geometry WxH  largest size clients may resize to; the X server
                            reserves this much [default: 1920x1200 or larger]
        --depth N           colour depth of the display rvnc starts [default: 24]
        --xserver PROG      X server to start, must take Xvfb's arguments
                            [default: Xvfb]
    -p, --password PASS     VNC password (max 8 characters, as the protocol allows)
        --password-file F   read the password from a file (first line)
        --no-password       serve with no authentication at all
        --allow-origin HOST hostname to accept in a browser Origin header, for
                            reverse proxies; repeatable
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
    max_geometry: Option<(u32, u32)>,
    depth: u32,
    xserver: String,
    password: Option<String>,
    no_password: bool,
    allowed_origins: Vec<String>,
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
            max_geometry: None,
            depth: 24,
            xserver: "Xvfb".into(),
            password: None,
            no_password: false,
            allowed_origins: Vec::new(),
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

    // Whatever happens next, do not leave a stray X server behind.
    let result = serve(&args, &sup);
    sup.stop();
    result
}

fn serve(args: &Args, sup: &Arc<Supervisor>) -> Result<(), Fail> {
    // Check the session command before starting an X server for it, so a
    // typo or a missing desktop does not leave a display running.
    if let Some(program) = args.command.first() {
        if spawn::find_program(program).is_none() {
            return Err(format!(
                "{program} not found. Install the desktop or program you want to run, \
                 or give rvnc a command that exists."
            )
            .into());
        }
    }

    // Work out which display to serve, starting one if we have to.
    let display = match (&args.display, args.command.is_empty()) {
        (Some(d), _) => d.clone(),
        (None, false) => {
            // The X server can never grow past the size it was started at, so
            // allocate the maximum and shrink to --geometry right after.
            let max = args.max_geometry.unwrap_or((
                args.geometry.0.max(1920),
                args.geometry.1.max(1200),
            ));
            spawn::start_xvfb(sup, &args.xserver, max.0, max.1, args.depth)?
        }
        (None, true) => std::env::var("DISPLAY").map_err(|_| {
            "no display to serve: pass --display, set DISPLAY, or give a command to run"
        })?,
    };

    // Shrink the fresh display to the requested starting size.
    if args.display.is_none() && !args.command.is_empty() {
        if let Some(r) = x11::Resizer::open(Some(&display)) {
            if let Err(e) = r.resize(args.geometry.0 as u16, args.geometry.1 as u16) {
                log::warn(&format!("could not set the initial size: {e}"));
            }
        }
    }

    if !args.command.is_empty() {
        spawn::start_session(sup, &display, &args.command)?;
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

    let resizer = x11::Resizer::open(Some(&display)).map(Arc::new);
    match &resizer {
        Some(r) => log::debug(&format!("resizing available up to {}x{}", r.max.0, r.max.1)),
        None => log::warn("no RandR on this display: clients cannot change the resolution"),
    }

    let hub = Hub::new(width, height);

    // Clipboard needs its own connection and an event loop of its own.
    let clipboard = match clipboard::Clipboard::open(Some(&display)) {
        Ok(c) => {
            let hub_for_clipboard = hub.clone();
            let runner = c.clone();
            std::thread::Builder::new()
                .name("clipboard".into())
                .spawn(move || {
                    let result = runner.run(move |text| {
                        let bytes = Arc::new(clipboard::to_latin1(&text));
                        hub_for_clipboard.broadcast_clipboard(bytes, None);
                    });
                    if let Err(e) = result {
                        log::warn(&format!("clipboard stopped: {e}"));
                    }
                })?;
            Some(c)
        }
        Err(e) => {
            log::warn(&format!("clipboard unavailable: {e}"));
            None
        }
    };
    let password = resolve_password(args)?;

    let cfg = Arc::new(Config {
        password: password.clone(),
        view_only: args.view_only,
        desktop_name: format!("rvnc {display}"),
        allowed_origins: args.allowed_origins.clone(),
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
        extras: rfb::Extras { resizer, clipboard },
    });
    http::run(listener, server)?;
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
            "--max-geometry" => {
                out.max_geometry = Some(parse_geometry(&need(args.next(), "--max-geometry")?)?)
            }
            "--xserver" => out.xserver = need(args.next(), "--xserver")?,
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
            "--allow-origin" => out
                .allowed_origins
                .push(need(args.next(), "--allow-origin")?),
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
    fn xserver_defaults_to_xvfb_and_is_overridable() {
        assert_eq!(parse(&[]).xserver, "Xvfb");
        assert_eq!(parse(&["--xserver", "/opt/bin/Xvfb"]).xserver, "/opt/bin/Xvfb");
    }

    #[test]
    fn help_does_not_claim_rvnc_is_an_x_server() {
        assert!(USAGE.contains("it is not an X server itself"));
        assert!(!USAGE.contains("start a virtual display"));
    }

    #[test]
    fn allow_origin_is_repeatable() {
        let a = parse(&["--allow-origin", "a.example", "--allow-origin", "b.example"]);
        assert_eq!(a.allowed_origins, vec!["a.example", "b.example"]);
        assert!(parse(&[]).allowed_origins.is_empty());
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
