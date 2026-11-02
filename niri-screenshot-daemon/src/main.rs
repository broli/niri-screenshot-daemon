use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread::{sleep, spawn};
use std::time::Duration;

use clap::Parser;
use serde::{Deserialize, Serialize};

const SCREENSHOT_DELAYS: [Duration; 3] = [
    Duration::from_millis(4),
    Duration::from_secs(1),
    Duration::from_secs(1),
];

#[derive(Parser, Debug)]
#[command(
    name = "niri-screenshot-daemon",
    about = "Lightweight Screenshot Watchdog for Niri (Host Daemon & Nested Client)"
)]
struct Cli {
    /// Run in client mode: register sockets with the running host daemon and exit
    #[arg(long)]
    client: bool,

    /// Extra socket path or directory to monitor (can be used multiple times)
    #[arg(short, long = "socket", value_name = "PATH")]
    sockets: Vec<PathBuf>,

    /// Path to the daemon control socket
    #[arg(long, value_name = "PATH")]
    control_socket: Option<PathBuf>,

    /// Path to a TOML configuration file
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Host `WAYLAND_DISPLAY` override
    #[arg(long, env = "WAYLAND_DISPLAY")]
    wayland_display: Option<String>,

    /// Host `XDG_RUNTIME_DIR` override
    #[arg(long, env = "XDG_RUNTIME_DIR")]
    runtime_dir: Option<PathBuf>,

    /// Optional background polling interval in seconds (0 = disabled, event-driven socket monitoring only)
    #[arg(long, default_value = "0", value_name = "SECS")]
    poll_interval: u64,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    extra_sockets: HashSet<PathBuf>,
    control_socket: Option<PathBuf>,
    host_wayland: Option<String>,
    host_runtime: Option<String>,
    poll_interval: Option<u64>,
}

struct Config {
    host_wayland: String,
    host_runtime: String,
    runtime_dir: PathBuf,
    extra_paths: HashSet<PathBuf>,
    poll_interval: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ControlRequest {
    AddSockets { sockets: Vec<PathBuf> },
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ControlResponse {
    Ok {
        added: Vec<PathBuf>,
        message: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Deserialize)]
struct ScreenshotCaptured {
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct NiriEvent {
    #[serde(rename = "ScreenshotCaptured", default)]
    screenshot_captured: Option<ScreenshotCaptured>,
}

struct SocketCleaner(PathBuf);
impl Drop for SocketCleaner {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn wait_for_screenshot_file(path: &Path, delays: &[Duration]) -> bool {
    for &delay in delays {
        sleep(delay);
        if path.exists() {
            return true;
        }
    }
    false
}

fn on_screenshot(path: &Path, config: &Config) {
    if !wait_for_screenshot_file(path, &SCREENSHOT_DELAYS) {
        eprintln!(
            "[niri-screenshot-daemon] Screenshot file not found after retries: {}",
            path.display()
        );
        return;
    }

    println!(
        "[niri-screenshot-daemon] Screenshot detected: {}. Launching satty on host ({})",
        path.display(),
        config.host_wayland
    );

    match Command::new("satty")
        .env("WAYLAND_DISPLAY", &config.host_wayland)
        .env("XDG_RUNTIME_DIR", &config.host_runtime)
        .arg("--filename")
        .arg(path)
        .arg("--output-filename")
        .arg(path)
        .args(["--copy-command", "wl-copy", "--early-exit", "copy,save"])
        .spawn()
    {
        Ok(_) => {}
        Err(err) => eprintln!(
            "[niri-screenshot-daemon] Failed to launch satty for {}: {err}",
            path.display()
        ),
    }
}

fn listen_to_socket(
    sock_path: &Path,
    active_sockets: &Arc<Mutex<HashSet<PathBuf>>>,
    config: &Arc<Config>,
) {
    println!(
        "[niri-screenshot-daemon] Connected to Niri socket: {}",
        sock_path.display()
    );

    match UnixStream::connect(sock_path) {
        Ok(mut stream) => {
            if let Err(err) = stream.write_all(b"\"EventStream\"\n") {
                eprintln!(
                    "[niri-screenshot-daemon] Failed to send handshake to {}: {err}",
                    sock_path.display()
                );
            } else {
                let reader = BufReader::new(stream);
                for line in reader.lines().map_while(Result::ok) {
                    if let Ok(event) = serde_json::from_str::<NiriEvent>(&line)
                        && let Some(captured) = event.screenshot_captured
                    {
                        let cfg = Arc::clone(config);
                        spawn(move || {
                            on_screenshot(&captured.path, &cfg);
                        });
                    }
                }
            }
        }
        Err(err) => {
            eprintln!(
                "[niri-screenshot-daemon] Failed to connect to {}: {err}",
                sock_path.display()
            );
        }
    }

    println!(
        "[niri-screenshot-daemon] Socket disconnected or closed: {}",
        sock_path.display()
    );
    active_sockets
        .lock()
        .expect("active_sockets lock poisoned")
        .remove(sock_path);
}

fn start_monitoring_socket(
    sock: PathBuf,
    active_sockets: &Arc<Mutex<HashSet<PathBuf>>>,
    config: &Arc<Config>,
) -> Option<std::thread::JoinHandle<()>> {
    let mut set = active_sockets.lock().expect("active_sockets lock poisoned");
    if set.insert(sock.clone()) {
        let set_clone = Arc::clone(active_sockets);
        let config_clone = Arc::clone(config);
        Some(spawn(move || {
            listen_to_socket(&sock, &set_clone, &config_clone);
        }))
    } else {
        None
    }
}

fn scan_dir_for_sockets(dir: &Path, sockets: &mut HashSet<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.map_while(Result::ok) {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.starts_with("niri")
                && Path::new(name)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("sock"))
            {
                sockets.insert(path);
            }
        }
    }
}

fn scan_sockets(runtime_dir: &Path, extra_paths: &HashSet<PathBuf>) -> HashSet<PathBuf> {
    let mut sockets = HashSet::new();

    if let Ok(val) = env::var("NIRI_SOCKET") {
        let p = PathBuf::from(val);
        if p.exists() {
            sockets.insert(p);
        }
    }

    scan_dir_for_sockets(runtime_dir, &mut sockets);

    for path in extra_paths {
        if path.is_dir() {
            scan_dir_for_sockets(path, &mut sockets);
        } else if path.exists() {
            sockets.insert(path.clone());
        }
    }

    sockets
}

fn wait_for_sockets(
    runtime_dir: &Path,
    extra_paths: &HashSet<PathBuf>,
    retries: usize,
    delay: Duration,
) -> HashSet<PathBuf> {
    let mut sockets = scan_sockets(runtime_dir, extra_paths);
    if !sockets.is_empty() {
        return sockets;
    }

    for attempt in 1..=retries {
        println!("[niri-screenshot-daemon] No sockets found yet, checking at {attempt}s mark...");
        sleep(delay);
        sockets = scan_sockets(runtime_dir, extra_paths);
        if !sockets.is_empty() {
            return sockets;
        }
    }

    sockets
}

fn default_config_path() -> Option<PathBuf> {
    if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        Some(PathBuf::from(config_home).join("niri-screenshot-daemon/config.toml"))
    } else {
        env::var("HOME")
            .ok()
            .map(|home| PathBuf::from(home).join(".config/niri-screenshot-daemon/config.toml"))
    }
}

fn resolve_control_socket_path(
    cli_override: Option<&Path>,
    file_override: Option<&Path>,
) -> PathBuf {
    if let Some(p) = cli_override {
        return p.to_path_buf();
    }
    if let Some(p) = file_override {
        return p.to_path_buf();
    }
    if let Ok(val) = env::var("NIRI_DAEMON_SOCKET") {
        return PathBuf::from(val);
    }
    let uid = rustix::process::getuid().as_raw();
    let runtime_path = PathBuf::from(format!("/run/user/{uid}"));
    if runtime_path.is_dir() {
        runtime_path.join("niri-screenshot-daemon.sock")
    } else {
        std::env::temp_dir().join(format!("niri-screenshot-daemon-{uid}.sock"))
    }
}

fn load_file_config(cli_config_path: Option<&Path>) -> FileConfig {
    let path = match cli_config_path {
        Some(p) => Some(p.to_path_buf()),
        None => default_config_path().filter(|p| p.exists()),
    };

    let Some(config_path) = path else {
        return FileConfig::default();
    };

    match fs::read_to_string(&config_path) {
        Ok(contents) => match toml::from_str::<FileConfig>(&contents) {
            Ok(cfg) => {
                println!(
                    "[niri-screenshot-daemon] Loaded configuration from {}",
                    config_path.display()
                );
                cfg
            }
            Err(err) => {
                eprintln!(
                    "[niri-screenshot-daemon] Failed to parse config file {}: {err}",
                    config_path.display()
                );
                FileConfig::default()
            }
        },
        Err(err) => {
            if cli_config_path.is_some() {
                eprintln!(
                    "[niri-screenshot-daemon] Could not read config file {}: {err}",
                    config_path.display()
                );
            }
            FileConfig::default()
        }
    }
}

fn handle_control_connection(
    mut stream: UnixStream,
    active_sockets: &Arc<Mutex<HashSet<PathBuf>>>,
    config: &Arc<Config>,
) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_ok() && !line.trim().is_empty() {
        let response = match serde_json::from_str::<ControlRequest>(line.trim()) {
            Ok(ControlRequest::AddSockets { sockets }) => {
                let mut added = Vec::new();
                for sock in sockets {
                    if start_monitoring_socket(sock.clone(), active_sockets, config).is_some() {
                        added.push(sock);
                    }
                }
                println!(
                    "[niri-screenshot-daemon] Client registered {} new socket(s): {:?}",
                    added.len(),
                    added
                );
                let message = format!("Successfully registered {} socket(s)", added.len());
                ControlResponse::Ok { added, message }
            }
            Ok(ControlRequest::Ping) => ControlResponse::Ok {
                added: Vec::new(),
                message: "pong".to_string(),
            },
            Err(err) => ControlResponse::Error {
                message: format!("Invalid request: {err}"),
            },
        };

        if let Ok(mut json) = serde_json::to_string(&response) {
            json.push('\n');
            let _ = stream.write_all(json.as_bytes());
        }
    }
}

fn run_control_server(
    listener: UnixListener,
    active_sockets: Arc<Mutex<HashSet<PathBuf>>>,
    config: Arc<Config>,
) {
    spawn(move || {
        for stream in listener.incoming().map_while(Result::ok) {
            handle_control_connection(stream, &active_sockets, &config);
        }
    });
}

fn run_client(cli: &Cli, control_socket_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut target_sockets = cli.sockets.clone();

    if target_sockets.is_empty()
        && let Ok(val) = env::var("NIRI_SOCKET")
    {
        let p = PathBuf::from(val);
        if p.exists() {
            target_sockets.push(p);
        }
    }

    if target_sockets.is_empty() {
        let uid = rustix::process::getuid().as_raw();
        let runtime_dir = env::var("XDG_RUNTIME_DIR")
            .map_or_else(|_| PathBuf::from(format!("/run/user/{uid}")), PathBuf::from);

        let empty_set = HashSet::new();
        target_sockets = wait_for_sockets(&runtime_dir, &empty_set, 3, Duration::from_secs(1))
            .into_iter()
            .collect();
    }

    if target_sockets.is_empty() {
        return Err(
            "[niri-screenshot-daemon] Client: No Niri sockets found to register. Exiting.".into(),
        );
    }

    println!(
        "[niri-screenshot-daemon] Client: Connecting to host daemon at {}...",
        control_socket_path.display()
    );

    let mut stream = UnixStream::connect(control_socket_path).map_err(|err| {
        format!(
            "Failed to connect to host daemon at {}: {err}. Ensure host daemon is running.",
            control_socket_path.display()
        )
    })?;

    let req = ControlRequest::AddSockets {
        sockets: target_sockets,
    };
    let mut payload = serde_json::to_string(&req)?;
    payload.push('\n');

    stream.write_all(payload.as_bytes())?;

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    if reader.read_line(&mut response_line).is_ok() {
        match serde_json::from_str::<ControlResponse>(response_line.trim())? {
            ControlResponse::Ok { added, message } => {
                println!("[niri-screenshot-daemon] Success: {message}");
                for sock in added {
                    println!("  -> Registered socket: {}", sock.display());
                }
                Ok(())
            }
            ControlResponse::Error { message } => {
                Err(format!("Host daemon error: {message}").into())
            }
        }
    } else {
        Err("No response received from host daemon.".into())
    }
}

fn bind_control_listener(
    control_socket_path: &Path,
) -> Result<UnixListener, Box<dyn std::error::Error>> {
    match UnixListener::bind(control_socket_path) {
        Ok(l) => Ok(l),
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(control_socket_path).is_ok() {
                return Err(format!(
                    "Host daemon is already running at {}. To register this session, run with --client.",
                    control_socket_path.display()
                )
                .into());
            }
            let _ = fs::remove_file(control_socket_path);
            Ok(UnixListener::bind(control_socket_path)?)
        }
        Err(err) => Err(err.into()),
    }
}

fn run_daemon(
    cli: &Cli,
    file_config: FileConfig,
    control_socket_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let uid = rustix::process::getuid().as_raw();
    let host_runtime = cli
        .runtime_dir
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .or(file_config.host_runtime)
        .unwrap_or_else(|| format!("/run/user/{uid}"));

    let host_wayland = cli
        .wayland_display
        .clone()
        .or(file_config.host_wayland)
        .unwrap_or_else(|| "wayland-0".to_string());

    let poll_interval = if cli.poll_interval > 0 {
        cli.poll_interval
    } else {
        file_config.poll_interval.unwrap_or(0)
    };

    let runtime_dir = PathBuf::from(&host_runtime);

    let mut extra_paths: HashSet<PathBuf> = file_config.extra_sockets;
    if let Ok(env_extra) = env::var("NIRI_EXTRA_SOCKETS") {
        for part in env_extra.split([':', ',']) {
            let trimmed = part.trim();
            if !trimmed.is_empty() {
                extra_paths.insert(PathBuf::from(trimmed));
            }
        }
    }
    for sock in &cli.sockets {
        extra_paths.insert(sock.clone());
    }

    let control_listener = bind_control_listener(control_socket_path)?;
    let _socket_cleaner = SocketCleaner(control_socket_path.to_path_buf());

    println!("============================================================");
    println!(" [niri-screenshot-daemon] Host Screenshot Watchdog ");
    println!(" Host Wayland:   {host_wayland} (Runtime: {host_runtime})");
    println!(" Control Socket: {}", control_socket_path.display());
    if !extra_paths.is_empty() {
        let paths_display = extra_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        println!(" Extra monitored paths: {paths_display}");
    }
    if poll_interval == 0 {
        println!(" Mode: Pure event-driven (zero background polling)");
    } else {
        println!(" Mode: Polling directories every {poll_interval}s");
    }
    println!("============================================================");

    let config = Arc::new(Config {
        host_wayland,
        host_runtime,
        runtime_dir,
        extra_paths,
        poll_interval,
    });

    let active_sockets: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));

    run_control_server(
        control_listener,
        Arc::clone(&active_sockets),
        Arc::clone(&config),
    );

    let initial_sockets = wait_for_sockets(
        &config.runtime_dir,
        &config.extra_paths,
        3,
        Duration::from_secs(1),
    );
    let mut host_handles = Vec::new();

    if initial_sockets.is_empty() {
        println!(
            "[niri-screenshot-daemon] No local sockets detected yet; waiting for client registrations..."
        );
    } else {
        for sock in initial_sockets {
            if let Some(handle) = start_monitoring_socket(sock, &active_sockets, &config) {
                host_handles.push(handle);
            }
        }
    }

    if config.poll_interval == 0 {
        if host_handles.is_empty() {
            std::thread::park();
        } else {
            for handle in host_handles {
                let _ = handle.join();
            }
            println!("[niri-screenshot-daemon] Host Niri session disconnected. Exiting daemon.");
        }
    } else {
        loop {
            let discovered = scan_sockets(&config.runtime_dir, &config.extra_paths);
            for sock in discovered {
                start_monitoring_socket(sock, &active_sockets, &config);
            }
            sleep(Duration::from_secs(config.poll_interval));
        }
    }

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let file_config = load_file_config(cli.config.as_deref());

    let control_socket_path = resolve_control_socket_path(
        cli.control_socket.as_deref(),
        file_config.control_socket.as_deref(),
    );

    if cli.client {
        run_client(&cli, &control_socket_path)
    } else {
        run_daemon(&cli, file_config, &control_socket_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_screenshot_captured() {
        let json = r#"{"ScreenshotCaptured":{"path":"/home/user/Pictures/screenshot.png"}}"#;
        let event: NiriEvent = serde_json::from_str(json).expect("should parse");
        assert_eq!(
            event
                .screenshot_captured
                .expect("expected captured event")
                .path,
            PathBuf::from("/home/user/Pictures/screenshot.png")
        );
    }

    #[test]
    fn test_deserialize_other_event() {
        let json = r#"{"WorkspaceActivated":{"id":1}}"#;
        let event: NiriEvent = serde_json::from_str(json).expect("should ignore other events");
        assert!(event.screenshot_captured.is_none());
    }

    #[test]
    fn test_parse_file_config() {
        let toml_str = r#"
            extra_sockets = ["/run/user/1001", "/tmp/another.sock"]
            control_socket = "/tmp/custom-control.sock"
            host_wayland = "wayland-1"
            host_runtime = "/run/user/1001"
            poll_interval = 30
        "#;
        let cfg: FileConfig = toml::from_str(toml_str).expect("valid config");
        assert_eq!(cfg.extra_sockets.len(), 2);
        assert!(cfg.extra_sockets.contains(&PathBuf::from("/run/user/1001")));
        assert_eq!(
            cfg.control_socket,
            Some(PathBuf::from("/tmp/custom-control.sock"))
        );
        assert_eq!(cfg.host_wayland.as_deref(), Some("wayland-1"));
        assert_eq!(cfg.host_runtime.as_deref(), Some("/run/user/1001"));
        assert_eq!(cfg.poll_interval, Some(30));
    }

    #[test]
    fn test_control_protocol_serialization() {
        let req = ControlRequest::AddSockets {
            sockets: vec![PathBuf::from("/tmp/nested-niri.sock")],
        };
        let json = serde_json::to_string(&req).expect("serialize req");
        let parsed: ControlRequest = serde_json::from_str(&json).expect("deserialize req");
        match parsed {
            ControlRequest::AddSockets { sockets } => {
                assert_eq!(sockets, vec![PathBuf::from("/tmp/nested-niri.sock")]);
            }
            ControlRequest::Ping => panic!("Expected AddSockets"),
        }
    }

    #[test]
    fn test_control_client_server_interaction() {
        let test_id = std::process::id();
        let sock_path = std::env::temp_dir().join(format!("test_ctrl_{test_id}.sock"));
        let _ = fs::remove_file(&sock_path);

        let listener = UnixListener::bind(&sock_path).expect("bind test listener");
        let active_sockets: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
        let config = Arc::new(Config {
            host_wayland: "wayland-0".into(),
            host_runtime: "/run/user/1000".into(),
            runtime_dir: PathBuf::from("/run/user/1000"),
            extra_paths: HashSet::new(),
            poll_interval: 0,
        });

        let active_clone = Arc::clone(&active_sockets);
        let config_clone = Arc::clone(&config);
        let server_handle = spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                handle_control_connection(stream, &active_clone, &config_clone);
            }
        });

        // Client side
        let mut client_stream = UnixStream::connect(&sock_path).expect("connect to test listener");
        let target_sock = PathBuf::from("/tmp/dummy_nested.sock");
        let req = ControlRequest::AddSockets {
            sockets: vec![target_sock.clone()],
        };
        let mut msg = serde_json::to_string(&req).unwrap();
        msg.push('\n');
        client_stream.write_all(msg.as_bytes()).unwrap();

        let mut reader = BufReader::new(client_stream);
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).unwrap();

        let resp: ControlResponse = serde_json::from_str(resp_line.trim()).unwrap();
        match resp {
            ControlResponse::Ok { added, .. } => {
                assert_eq!(added, vec![target_sock]);
            }
            ControlResponse::Error { .. } => panic!("Expected Ok response"),
        }

        server_handle.join().unwrap();
        let _ = fs::remove_file(&sock_path);
    }

    #[test]
    fn test_wait_for_screenshot_file_fast() {
        let nonexistent = PathBuf::from("/tmp/does_not_exist_screenshot_test_12345.png");
        assert!(!wait_for_screenshot_file(
            &nonexistent,
            &[Duration::from_millis(1), Duration::from_millis(1)]
        ));
    }

    #[test]
    fn test_wait_for_screenshot_file_immediate() {
        let temp_file = std::env::temp_dir().join(format!("test_ss_{}.png", std::process::id()));
        fs::write(&temp_file, b"test").unwrap();
        assert!(wait_for_screenshot_file(&temp_file, &SCREENSHOT_DELAYS));
        let _ = fs::remove_file(&temp_file);
    }
}
