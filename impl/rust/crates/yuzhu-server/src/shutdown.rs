//! Shutdown coordination (`m2.md` 5.7 / 6.12): the registry of live
//! connections and the requested shutdown mode.
//!
//! - smart (`SIGTERM`): refuse new connections, wait for existing ones.
//! - fast (`SIGINT`): refuse new connections, ask every session to stop and
//!   wake the ones blocked in a read, then wait.
//! - immediate (`SIGQUIT`): exit without a checkpoint.

use std::collections::HashMap;
use std::net::{Shutdown as SocketShutdown, TcpStream};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use yuzhu_core::InterruptFlag;

/// How the server is asked to stop. Ordered by severity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ShutdownMode {
    Smart = 1,
    Fast = 2,
    Immediate = 3,
}

impl ShutdownMode {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Smart),
            2 => Some(Self::Fast),
            3 => Some(Self::Immediate),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct Entry {
    stream: TcpStream,
    flag: Option<Arc<InterruptFlag>>,
    /// `(backend pid, cancel secret)` shown to the client in `BackendKeyData`.
    key: Option<(i32, i32)>,
}

#[derive(Debug, Default)]
struct Registry {
    /// No new connection may register any more.
    closed: bool,
    /// Every registered connection has been told to stop.
    interrupted: bool,
    conns: HashMap<i32, Entry>,
}

/// Shared between the accept loop, the signal thread and the connections.
#[derive(Debug, Default)]
pub struct Coordinator {
    level: AtomicU8,
    registry: Mutex<Registry>,
}

impl Coordinator {
    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Asks the server to stop. A stronger mode replaces a weaker one; a
    /// weaker request after a stronger one is ignored.
    pub fn request(&self, mode: ShutdownMode) {
        self.level.fetch_max(mode as u8, Ordering::SeqCst);
    }

    /// The strongest mode requested so far.
    pub fn requested(&self) -> Option<ShutdownMode> {
        ShutdownMode::from_u8(self.level.load(Ordering::SeqCst))
    }

    /// Registers a connection. `None` once [`Coordinator::close`] was called.
    pub fn register(self: &Arc<Self>, pid: i32, stream: TcpStream) -> Option<Registration> {
        let mut r = self.registry();
        if r.closed {
            return None;
        }
        r.conns.insert(
            pid,
            Entry {
                stream,
                flag: None,
                key: None,
            },
        );
        Some(Registration {
            coordinator: Arc::clone(self),
            pid,
        })
    }

    /// Refuses new registrations from now on.
    pub fn close(&self) {
        self.registry().closed = true;
    }

    /// Tells every registered connection to stop: sets its interrupt flag
    /// and shuts down the read side of the socket so that a blocked read
    /// returns EOF. Connections registering later (none, once closed) or
    /// attaching their flag later are handled by [`Registration::set_flag`].
    pub fn interrupt_all(&self) {
        let mut r = self.registry();
        r.interrupted = true;
        for e in r.conns.values() {
            if let Some(f) = &e.flag {
                f.request_terminate();
            }
            let _ = e.stream.shutdown(SocketShutdown::Read);
        }
    }

    /// Handles a `CancelRequest`: if a session with this backend pid and
    /// secret exists, asks it to cancel its current statement. Returns
    /// whether a session matched (the client is never told).
    pub fn cancel(&self, pid: i32, secret: i32) -> bool {
        let r = self.registry();
        let hit = r.conns.values().find(|e| e.key == Some((pid, secret)));
        match hit.and_then(|e| e.flag.as_ref()) {
            Some(f) => {
                f.request_cancel();
                true
            }
            None => false,
        }
    }

    /// Number of registered connections.
    pub fn len(&self) -> usize {
        self.registry().conns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A connection's entry in the registry; removed on drop.
#[derive(Debug)]
pub struct Registration {
    coordinator: Arc<Coordinator>,
    pid: i32,
}

impl Registration {
    /// Attaches the session's interrupt flag. If a fast shutdown already
    /// happened, the flag is raised at once.
    pub fn set_flag(&self, flag: Arc<InterruptFlag>) {
        let mut r = self.coordinator.registry();
        let interrupted = r.interrupted;
        if interrupted {
            flag.request_terminate();
        }
        if let Some(e) = r.conns.get_mut(&self.pid) {
            e.flag = Some(flag);
        }
    }
}

impl Registration {
    /// Records the key sent in `BackendKeyData` so that a `CancelRequest`
    /// can find this session.
    pub fn set_key(&self, backend_pid: i32, secret: i32) {
        if let Some(e) = self.coordinator.registry().conns.get_mut(&self.pid) {
            e.key = Some((backend_pid, secret));
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.coordinator.registry().conns.remove(&self.pid);
    }
}

/// A cloneable handle for requesting a shutdown (signal thread, tests).
#[derive(Debug, Clone)]
pub struct ShutdownHandle {
    coordinator: Arc<Coordinator>,
}

impl ShutdownHandle {
    pub(crate) fn new(coordinator: Arc<Coordinator>) -> Self {
        Self { coordinator }
    }

    pub fn request(&self, mode: ShutdownMode) {
        self.coordinator.request(mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (s, _) = l.accept().unwrap();
        (c, s)
    }

    #[test]
    fn mode_escalates_and_never_downgrades() {
        let c = Coordinator::default();
        assert_eq!(c.requested(), None);
        c.request(ShutdownMode::Smart);
        assert_eq!(c.requested(), Some(ShutdownMode::Smart));
        c.request(ShutdownMode::Immediate);
        c.request(ShutdownMode::Fast);
        assert_eq!(c.requested(), Some(ShutdownMode::Immediate));
    }

    #[test]
    fn register_and_drop() {
        let c = Arc::new(Coordinator::default());
        let (_client, server) = pair();
        let reg = c.register(1, server).unwrap();
        assert_eq!(c.len(), 1);
        drop(reg);
        assert!(c.is_empty());
    }

    #[test]
    fn close_refuses_new_connections() {
        let c = Arc::new(Coordinator::default());
        c.close();
        let (_client, server) = pair();
        assert!(c.register(1, server).is_none());
    }

    #[test]
    fn interrupt_sets_flag_and_wakes_reader() {
        let c = Arc::new(Coordinator::default());
        let (_client, server) = pair();
        let mut reader = server.try_clone().unwrap();
        let reg = c.register(7, server).unwrap();
        let flag = Arc::new(InterruptFlag::default());
        reg.set_flag(Arc::clone(&flag));
        c.close();
        c.interrupt_all();
        assert!(flag.is_terminate_requested());
        let mut buf = [0u8; 1];
        assert_eq!(reader.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn cancel_needs_matching_pid_and_secret() {
        let c = Arc::new(Coordinator::default());
        let (_client, server) = pair();
        let reg = c.register(1, server).unwrap();
        let flag = Arc::new(InterruptFlag::default());
        reg.set_flag(Arc::clone(&flag));
        assert!(!c.cancel(42, 7), "key not registered yet");
        reg.set_key(42, 7);
        assert!(!c.cancel(42, 8), "wrong secret");
        assert!(!c.cancel(41, 7), "wrong pid");
        assert!(flag.check().is_ok());
        assert!(c.cancel(42, 7));
        assert_eq!(flag.check().unwrap_err().sqlstate.0, "57014");
        drop(reg);
        assert!(!c.cancel(42, 7), "gone after the connection ended");
    }

    #[test]
    fn flag_attached_after_interrupt_is_raised() {
        let c = Arc::new(Coordinator::default());
        let (_client, server) = pair();
        let reg = c.register(7, server).unwrap();
        c.close();
        c.interrupt_all();
        let flag = Arc::new(InterruptFlag::default());
        reg.set_flag(Arc::clone(&flag));
        assert!(flag.is_terminate_requested());
    }
}
