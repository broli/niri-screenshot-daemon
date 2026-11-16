use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::spawn;
use std::time::{Duration, Instant};

use zbus::MatchRule;
use zbus::blocking::{Connection, MessageIterator, connection::Builder};
use zbus::message::Type as MessageType;
use zbus::zvariant::Value;

use crate::Config;

const NOTIFICATIONS_DEST: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
const NOTIFICATIONS_IFACE: &str = "org.freedesktop.Notifications";

const MAX_PENDING: usize = 32;
const PENDING_TTL: Duration = Duration::from_secs(300);

const REASON_DISMISSED: u32 = 2;
const REASON_CLOSED_BY_CALL: u32 = 3;

#[derive(Debug, Default)]
struct PendingNotifications(VecDeque<(u32, PathBuf, Instant)>);

impl PendingNotifications {
    fn prune(&mut self, now: Instant) {
        while let Some((_, _, ts)) = self.0.front() {
            if now.duration_since(*ts) > PENDING_TTL || self.0.len() > MAX_PENDING {
                self.0.pop_front();
            } else {
                break;
            }
        }
    }

    fn insert(&mut self, id: u32, path: PathBuf, now: Instant) {
        self.0.retain(|(existing, _, _)| *existing != id);
        self.0.push_back((id, path, now));
        self.prune(now);
    }

    fn take(&mut self, id: u32, now: Instant) -> Option<PathBuf> {
        self.prune(now);
        let pos = self.0.iter().position(|(existing, _, _)| *existing == id)?;
        self.0.remove(pos).map(|(_, path, _)| path)
    }

    fn remove(&mut self, id: u32) {
        self.0.retain(|(existing, _, _)| *existing != id);
    }
}

#[derive(Clone)]
pub(crate) struct Notifier {
    config: Arc<Config>,
    prefer_host_runtime_bus: bool,
    pending: Arc<Mutex<PendingNotifications>>,
    connection: Arc<(Mutex<Option<Connection>>, Condvar)>,
}

impl Notifier {
    pub(crate) fn new(config: Arc<Config>, prefer_host_runtime_bus: bool) -> Self {
        Self {
            config,
            prefer_host_runtime_bus,
            pending: Arc::new(Mutex::new(PendingNotifications::default())),
            connection: Arc::new((Mutex::new(None), Condvar::new())),
        }
    }

    fn connect_session(
        runtime_dir: &Path,
        prefer_host_runtime_bus: bool,
    ) -> zbus::Result<Connection> {
        let runtime_bus = || {
            let addr = format!("unix:path={}", runtime_dir.join("bus").display());
            Builder::address(addr.as_str())?.build()
        };

        if prefer_host_runtime_bus {
            runtime_bus().or_else(|_| Connection::session())
        } else {
            Connection::session().or_else(|_| runtime_bus())
        }
    }

    fn get_or_connect(&self) -> zbus::Result<Connection> {
        let mut guard = self
            .connection
            .0
            .lock()
            .expect("dbus connection lock poisoned");
        if let Some(conn) = guard.as_ref() {
            return Ok(conn.clone());
        }
        let conn = Self::connect_session(&self.config.runtime_dir, self.prefer_host_runtime_bus)?;
        *guard = Some(conn.clone());
        self.connection.1.notify_one();
        Ok(conn)
    }

    fn reset_connection(&self, failed: &Connection) {
        let mut guard = self
            .connection
            .0
            .lock()
            .expect("dbus connection lock poisoned");
        if guard
            .as_ref()
            .is_some_and(|cur| cur.unique_name() == failed.unique_name())
        {
            *guard = None;
        }
    }

    pub(crate) fn start_listener(&self) {
        let notifier = self.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<()>(1);
        spawn(move || {
            let mut ready_tx = Some(ready_tx);
            loop {
                match notifier.get_or_connect() {
                    Ok(conn) => {
                        match notifier.subscribe_signals(&conn) {
                            Ok(iter) => {
                                if let Some(tx) = ready_tx.take() {
                                    let _ = tx.send(());
                                }
                                if let Err(err) = notifier.process_signals(iter) {
                                    eprintln!(
                                        "[niri-screenshot-daemon] D-Bus notification listener disconnected: {err}"
                                    );
                                }
                            }
                            Err(err) => {
                                if let Some(tx) = ready_tx.take() {
                                    let _ = tx.send(());
                                }
                                eprintln!(
                                    "[niri-screenshot-daemon] Failed to subscribe to D-Bus notification signals: {err}"
                                );
                            }
                        }
                        notifier.reset_connection(&conn);
                    }
                    Err(err) => {
                        if let Some(tx) = ready_tx.take() {
                            let _ = tx.send(());
                        }
                        eprintln!(
                            "[niri-screenshot-daemon] D-Bus session bus unavailable ({err}); retrying in 5s..."
                        );
                    }
                }
                let guard = notifier
                    .connection
                    .0
                    .lock()
                    .expect("dbus connection lock poisoned");
                let _ = notifier.connection.1.wait_timeout_while(
                    guard,
                    Duration::from_secs(5),
                    |conn| conn.is_none(),
                );
            }
        });
        let _ = ready_rx.recv();
    }

    fn subscribe_signals(&self, conn: &Connection) -> zbus::Result<MessageIterator> {
        let _ = self;
        let rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .sender(NOTIFICATIONS_DEST)?
            .interface(NOTIFICATIONS_IFACE)?
            .path(NOTIFICATIONS_PATH)?
            .build();

        MessageIterator::for_match_rule(rule, conn, Some(64))
    }

    fn process_signals(&self, iter: MessageIterator) -> zbus::Result<()> {
        for msg_res in iter {
            let msg = msg_res?;
            let header = msg.header();
            let Some(member) = header.member() else {
                continue;
            };
            match member.as_str() {
                "ActionInvoked" => {
                    if let Ok((id, action_key)) = msg.body().deserialize::<(u32, String)>()
                        && action_key == "default"
                        && let Some(path) = self
                            .pending
                            .lock()
                            .expect("pending notifications lock poisoned")
                            .take(id, Instant::now())
                    {
                        let cfg = Arc::clone(&self.config);
                        spawn(move || crate::launch_satty(&path, &cfg));
                    }
                }
                "NotificationClosed" => {
                    if let Ok((id, reason)) = msg.body().deserialize::<(u32, u32)>()
                        && (reason == REASON_DISMISSED || reason == REASON_CLOSED_BY_CALL)
                    {
                        self.pending
                            .lock()
                            .expect("pending notifications lock poisoned")
                            .remove(id);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(crate) fn notify_screenshot(
        &self,
        path: &Path,
        copied_to_clipboard: bool,
    ) -> Result<u32, String> {
        let conn = self
            .get_or_connect()
            .map_err(|err| format!("D-Bus session connect failed: {err}"))?;

        let file_uri = path_to_file_uri(path);
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("screenshot.png");
        let status_note = if copied_to_clipboard {
            "Copied to clipboard"
        } else {
            "Saved to disk"
        };
        let body = format!("{filename}\n{status_note} — click to annotate");

        let hints = HashMap::from([
            ("image-path", Value::from(file_uri.as_str())),
            ("urgency", Value::U8(1)),
            ("desktop-entry", Value::from("niri-screenshot-daemon")),
            ("category", Value::from("screenshot")),
        ]);

        let reply = conn
            .call_method(
                Some(NOTIFICATIONS_DEST),
                NOTIFICATIONS_PATH,
                Some(NOTIFICATIONS_IFACE),
                "Notify",
                &(
                    "niri-screenshot-daemon",
                    0u32,
                    "camera-photo",
                    "Screenshot captured",
                    body.as_str(),
                    ["default", "Annotate"].as_slice(),
                    hints,
                    self.config.notify_timeout,
                ),
            )
            .map_err(|err| {
                self.reset_connection(&conn);
                format!("D-Bus Notify call failed: {err}")
            })?;

        let id: u32 = reply
            .body()
            .deserialize()
            .map_err(|err| format!("Invalid D-Bus Notify reply: {err}"))?;

        self.pending
            .lock()
            .expect("pending notifications lock poisoned")
            .insert(id, path.to_path_buf(), Instant::now());

        Ok(id)
    }
}

pub(crate) fn path_to_file_uri(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut uri = String::with_capacity("file://".len() + raw.len());
    uri.push_str("file://");
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            uri.push(byte as char);
        } else {
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    uri
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_to_file_uri_encoding() {
        assert_eq!(
            path_to_file_uri(Path::new("/home/user/Pictures/Screenshot 100% #1?.png")),
            "file:///home/user/Pictures/Screenshot%20100%25%20%231%3F.png"
        );
        assert_eq!(
            path_to_file_uri(Path::new("/tmp/Captura á.png")),
            "file:///tmp/Captura%20%C3%A1.png"
        );
    }

    #[test]
    fn test_pending_notifications_insert_and_take() {
        let mut pending = PendingNotifications::default();
        let now = Instant::now();
        let path = PathBuf::from("/tmp/ss.png");

        pending.insert(42, path.clone(), now);
        assert_eq!(pending.take(42, now), Some(path));
        assert!(pending.take(42, now).is_none());
    }

    #[test]
    fn test_pending_notifications_eviction_and_remove() {
        let mut pending = PendingNotifications::default();
        let now = Instant::now();
        let max_pending = u32::try_from(MAX_PENDING).expect("MAX_PENDING fits in u32");

        for i in 1..=(max_pending + 5) {
            pending.insert(i, PathBuf::from(format!("/tmp/{i}.png")), now);
        }
        assert!(pending.take(1, now).is_none());
        assert!(pending.take(max_pending + 5, now).is_some());

        pending.remove(max_pending + 4);
        assert!(pending.take(max_pending + 4, now).is_none());
    }
}
