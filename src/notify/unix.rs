//! Linux and macOS: one Rustcord per data dir over a Unix socket
//! (`<XDG_RUNTIME_DIR or temp>/Rustcord-<hash>.sock`, one line per connection: `show` or
//! `quit`), and notifications on the thread "rustcord-notify", rate limited like the Windows
//! balloons (`Coalescer`), delivered by `linux::deliver` / `macos::deliver`.

use super::{Coalescer, TrayAction, os, push};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

static QUEUE: Mutex<Option<Sender<(String, String)>>> = Mutex::new(None);
/// This process bound the socket (only then `shutdown` removes it).
static OWNED: AtomicBool = AtomicBool::new(false);

pub fn start() {
    let (tx, rx) = mpsc::channel();
    if std::thread::Builder::new().name("rustcord-notify".into()).spawn(move || run(&rx)).is_ok() {
        *QUEUE.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    }
    os::start_tray();
}

pub fn show(title: &str, body: &str) {
    if let Some(tx) = QUEUE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let _ = tx.send((title.to_owned(), body.to_owned()));
    }
}

pub fn shutdown() {
    if OWNED.swap(false, Ordering::SeqCst) {
        let _ = std::fs::remove_file(socket_path());
    }
}

/// Shows what arrives, the rest summed up once the gap has passed; idle on the queue otherwise.
fn run(rx: &Receiver<(String, String)>) {
    let mut c = Coalescer::default();
    loop {
        let msg = match c.next_flush(Instant::now()) {
            Some(wait) => rx.recv_timeout(wait),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        let now = Instant::now();
        let shown = match msg {
            Ok((title, body)) => c.push(now, title, body),
            Err(RecvTimeoutError::Timeout) => c.flush(now),
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if let Some((title, body)) = shown {
            os::deliver(&title, if body.trim().is_empty() { "New message" } else { &body });
        }
    }
}

fn socket_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_dir());
    runtime.unwrap_or_else(std::env::temp_dir).join(format!("{}.sock", super::my_instance_name()))
}

pub fn claim_instance() -> bool {
    claim(&socket_path(), push)
}

/// True when nobody answers on `path`; this process then listens there (`on` gets each line's
/// action). A socket file nobody listens on is left over from a crash and replaced.
fn claim(path: &Path, on: impl Fn(TrayAction) + Send + 'static) -> bool {
    if UnixStream::connect(path).is_ok() {
        return false;
    }
    let _ = std::fs::remove_file(path);
    // Not bindable at all: better two windows than none (as on Windows).
    let Ok(listener) = UnixListener::bind(path) else { return true };
    OWNED.store(true, Ordering::SeqCst);
    let accept = move || {
        for conn in listener.incoming().flatten() {
            let _ = conn.set_read_timeout(Some(Duration::from_secs(1)));
            let mut line = String::new();
            if BufReader::new(conn).take(16).read_line(&mut line).is_ok() {
                match line.trim() {
                    "show" => on(TrayAction::Show),
                    "quit" => on(TrayAction::Quit),
                    _ => {}
                }
            }
        }
    };
    let _ = std::thread::Builder::new().name("rustcord-instance".into()).spawn(accept);
    true
}

/// Asks the running one to show itself, or to quit (then waits until it is gone, at most 20 s).
pub fn signal_running(quit: bool) -> bool {
    let path = socket_path();
    // The running one may still be starting.
    for _ in 0..50 {
        if send(&path, if quit { "quit" } else { "show" }) {
            for _ in 0..200 {
                if !quit || UnixStream::connect(&path).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn send(path: &Path, line: &str) -> bool {
    UnixStream::connect(path).and_then(|mut s| s.write_all(format!("{line}\n").as_bytes())).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_instance_claims_and_signals() {
        let path = std::env::temp_dir().join(format!("rc-instance-{}.sock", std::process::id()));
        // A socket file nobody listens on (a crashed run): replaced.
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let (tx, rx) = mpsc::channel();
        let on = move |a| {
            let _ = tx.send(a);
        };
        assert!(claim(&path, on.clone()), "first claim");
        assert!(!claim(&path, on), "second claim: the first one answers");
        assert!(send(&path, "show") && send(&path, "nonsense") && send(&path, "quit"));
        let got: Vec<TrayAction> = (0..2).map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap()).collect();
        assert_eq!(got, [TrayAction::Show, TrayAction::Quit]);
        std::fs::remove_file(&path).unwrap();
    }

    /// `--after-update`: the new copy waits while the old one still listens, then takes over.
    #[test]
    fn claim_waits_for_the_old_instance() {
        let path = std::env::temp_dir().join(format!("rc-instance-wait-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let old = UnixListener::bind(&path).unwrap();
        assert!(!super::super::retry_for(Duration::ZERO, || claim(&path, |_| {})), "the old one still answers");
        let quits = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(old);
        });
        assert!(super::super::retry_for(Duration::from_secs(10), || claim(&path, |_| {})));
        quits.join().unwrap();
        assert!(UnixStream::connect(&path).is_ok(), "the new one listens");
        std::fs::remove_file(&path).unwrap();
    }
}
