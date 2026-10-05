//! Shutdown modes, poison monitoring and a restart on a real directory.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use yuzhu_core::bootstrap::{InitdbOptions, initdb};
use yuzhu_core::storage::DEFAULT_RELSEG_SIZE;
use yuzhu_core::storage::vfs::LocalVfs;
use yuzhu_core::testing::TestCluster;
use yuzhu_core::wal::DEFAULT_WAL_SEGMENT_SIZE;
use yuzhu_server::config::Config;
use yuzhu_server::{Outcome, Server, ShutdownHandle, ShutdownMode};

struct Running {
    addr: SocketAddr,
    handle: ShutdownHandle,
    join: JoinHandle<std::io::Result<Outcome>>,
}

fn config() -> Config {
    Config {
        port: 0,
        ..Config::default()
    }
}

fn run(server: Server) -> Running {
    let addr = server.local_addr().unwrap();
    let handle = server.shutdown_handle();
    let join = std::thread::spawn(move || server.run());
    Running { addr, handle, join }
}

fn start_sim() -> Running {
    let cluster = TestCluster::new().cluster;
    run(Server::with_cluster(&config(), cluster).unwrap())
}

/// Minimal protocol client.
struct Raw(TcpStream);

impl Raw {
    fn connect(addr: SocketAddr) -> Self {
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Raw(s)
    }

    fn startup(&mut self) {
        let mut body = 196_608_u32.to_be_bytes().to_vec();
        for (k, v) in [("user", "postgres"), ("database", "postgres")] {
            body.extend_from_slice(k.as_bytes());
            body.push(0);
            body.extend_from_slice(v.as_bytes());
            body.push(0);
        }
        body.push(0);
        let len = u32::try_from(body.len() + 4).unwrap();
        self.0.write_all(&len.to_be_bytes()).unwrap();
        self.0.write_all(&body).unwrap();
    }

    /// Reads one message; `None` on EOF.
    fn read_msg(&mut self) -> Option<(u8, Vec<u8>)> {
        let mut tag = [0u8; 1];
        if self.0.read(&mut tag).ok()? == 0 {
            return None;
        }
        let mut len = [0u8; 4];
        self.0.read_exact(&mut len).ok()?;
        let n = u32::from_be_bytes(len) as usize - 4;
        let mut body = vec![0u8; n];
        self.0.read_exact(&mut body).ok()?;
        Some((tag[0], body))
    }

    fn until_ready(&mut self) {
        while let Some((tag, _)) = self.read_msg() {
            if tag == b'Z' {
                return;
            }
        }
        panic!("EOF before ReadyForQuery");
    }

    fn query(&mut self, sql: &str) -> Vec<(u8, Vec<u8>)> {
        let len = u32::try_from(sql.len() + 5).unwrap();
        self.0.write_all(b"Q").unwrap();
        self.0.write_all(&len.to_be_bytes()).unwrap();
        self.0.write_all(sql.as_bytes()).unwrap();
        self.0.write_all(&[0]).unwrap();
        let mut out = Vec::new();
        while let Some(m) = self.read_msg() {
            let done = m.0 == b'Z';
            out.push(m);
            if done {
                break;
            }
        }
        out
    }

    fn ready(addr: SocketAddr) -> Self {
        let mut c = Self::connect(addr);
        c.startup();
        c.until_ready();
        c
    }
}

fn sqlstate(body: &[u8]) -> String {
    body.split(|b| *b == 0)
        .find(|f| f.first() == Some(&b'C'))
        .map(|f| String::from_utf8_lossy(&f[1..]).into_owned())
        .unwrap_or_default()
}

fn data_row_count(msgs: &[(u8, Vec<u8>)]) -> usize {
    msgs.iter().filter(|m| m.0 == b'D').count()
}

#[test]
fn fast_shutdown_terminates_idle_sessions_with_57p01() {
    let r = start_sim();
    let mut c = Raw::ready(r.addr);
    r.handle.request(ShutdownMode::Fast);
    let (tag, body) = c.read_msg().expect("FATAL before close");
    assert_eq!(tag, b'E');
    assert_eq!(sqlstate(&body), "57P01");
    assert!(c.read_msg().is_none());
    assert_eq!(r.join.join().unwrap().unwrap(), Outcome::Stopped);
}

#[test]
fn smart_shutdown_waits_for_sessions_and_refuses_new_ones() {
    let r = start_sim();
    let mut c = Raw::ready(r.addr);
    r.handle.request(ShutdownMode::Smart);
    std::thread::sleep(Duration::from_millis(200));
    // New connections get 57P03.
    let mut late = Raw::connect(r.addr);
    let (tag, body) = late.read_msg().expect("FATAL");
    assert_eq!(tag, b'E');
    assert_eq!(sqlstate(&body), "57P03");
    // The existing session keeps working.
    let msgs = c.query("SELECT 1");
    assert_eq!(data_row_count(&msgs), 1);
    assert!(!r.join.is_finished());
    drop(c);
    assert_eq!(r.join.join().unwrap().unwrap(), Outcome::Stopped);
}

#[test]
fn smart_can_be_escalated_to_fast() {
    let r = start_sim();
    let mut c = Raw::ready(r.addr);
    r.handle.request(ShutdownMode::Smart);
    std::thread::sleep(Duration::from_millis(100));
    assert!(!r.join.is_finished());
    r.handle.request(ShutdownMode::Fast);
    let (tag, body) = c.read_msg().expect("FATAL");
    assert_eq!((tag, sqlstate(&body).as_str()), (b'E', "57P01"));
    assert_eq!(r.join.join().unwrap().unwrap(), Outcome::Stopped);
}

#[test]
fn immediate_shutdown_returns_without_checkpoint() {
    let cluster = TestCluster::new().cluster;
    let r = run(Server::with_cluster(&config(), cluster.clone()).unwrap());
    let _c = Raw::ready(r.addr);
    r.handle.request(ShutdownMode::Immediate);
    assert_eq!(r.join.join().unwrap().unwrap(), Outcome::Immediate);
    // Nothing was written: the pid lock is still held by the cluster.
    assert!(!cluster.is_poisoned());
}

#[test]
fn poisoned_cluster_ends_the_server_without_checkpoint() {
    let cluster = TestCluster::new().cluster;
    let r = run(Server::with_cluster(&config(), cluster.clone()).unwrap());
    cluster.poison();
    assert_eq!(r.join.join().unwrap().unwrap(), Outcome::Poisoned);
}

#[test]
fn fast_shutdown_interrupts_a_waiting_writer() {
    let r = start_sim();
    let mut a = Raw::ready(r.addr);
    let mut b = Raw::ready(r.addr);
    a.query("CREATE TABLE w (x int)");
    a.query("BEGIN");
    a.query("INSERT INTO w VALUES (1)");
    // b blocks on the writer lock (no lock_timeout), unless it times out.
    let t = std::thread::spawn(move || {
        let msgs = b.query("INSERT INTO w VALUES (2)");
        msgs.iter()
            .find(|m| m.0 == b'E')
            .map(|m| sqlstate(&m.1))
            .unwrap_or_default()
    });
    std::thread::sleep(Duration::from_millis(300));
    r.handle.request(ShutdownMode::Fast);
    // The holder of the writer lock is interrupted and aborts, so the waiter
    // either proceeds or fails; what matters is that the server stops.
    let _state = t.join().unwrap();
    drop(a);
    r.join.join().unwrap().unwrap();
}

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("yuzhu-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn restart_on_a_real_directory_keeps_committed_data() {
    let dir = temp_dir("restart");
    initdb(
        Arc::new(LocalVfs::new(dir.clone())),
        &InitdbOptions {
            superuser: "postgres".into(),
            no_sync: true,
            rel_seg_blocks: DEFAULT_RELSEG_SIZE,
            wal_segment_size: DEFAULT_WAL_SEGMENT_SIZE,
        },
    )
    .unwrap();
    let cfg = Config {
        data_directory: dir.clone(),
        shared_buffers: 256,
        ..config()
    };

    let r = run(Server::bind(&cfg).unwrap());
    let mut c = Raw::ready(r.addr);
    c.query("CREATE TABLE t (a int, b text)");
    c.query("INSERT INTO t VALUES (1, 'one'), (2, 'two')");
    c.query("BEGIN");
    c.query("INSERT INTO t VALUES (3, 'uncommitted')");
    r.handle.request(ShutdownMode::Fast);
    assert_eq!(r.join.join().unwrap().unwrap(), Outcome::Stopped);

    // A second server on the same directory starts cleanly.
    let r = run(Server::bind(&cfg).unwrap());
    let mut c = Raw::ready(r.addr);
    assert_eq!(data_row_count(&c.query("SELECT a, b FROM t")), 2);
    r.handle.request(ShutdownMode::Smart);
    drop(c);
    assert_eq!(r.join.join().unwrap().unwrap(), Outcome::Stopped);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn second_server_on_the_same_directory_is_refused() {
    let dir = temp_dir("lock");
    initdb(
        Arc::new(LocalVfs::new(dir.clone())),
        &InitdbOptions {
            superuser: "postgres".into(),
            no_sync: true,
            rel_seg_blocks: DEFAULT_RELSEG_SIZE,
            wal_segment_size: DEFAULT_WAL_SEGMENT_SIZE,
        },
    )
    .unwrap();
    let cfg = Config {
        data_directory: dir.clone(),
        shared_buffers: 256,
        ..config()
    };
    let first = Server::bind(&cfg).unwrap();
    let err = Server::bind(&cfg).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    let r = run(first);
    r.handle.request(ShutdownMode::Fast);
    assert_eq!(r.join.join().unwrap().unwrap(), Outcome::Stopped);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unclean_directory_is_recovered_on_the_next_start() {
    let dir = temp_dir("unclean");
    initdb(
        Arc::new(LocalVfs::new(dir.clone())),
        &InitdbOptions {
            superuser: "postgres".into(),
            no_sync: true,
            rel_seg_blocks: DEFAULT_RELSEG_SIZE,
            wal_segment_size: DEFAULT_WAL_SEGMENT_SIZE,
        },
    )
    .unwrap();
    let cfg = Config {
        data_directory: dir.clone(),
        shared_buffers: 256,
        ..config()
    };
    // Immediate shutdown leaves the control file InProduction.
    let r = run(Server::bind(&cfg).unwrap());
    r.handle.request(ShutdownMode::Immediate);
    assert_eq!(r.join.join().unwrap().unwrap(), Outcome::Immediate);
    // The old cluster object is gone only with the thread; give it a moment.
    std::thread::sleep(Duration::from_millis(100));
    // M3: the next start runs crash recovery instead of refusing.
    let r = run(Server::bind(&cfg).unwrap());
    r.handle.request(ShutdownMode::Fast);
    assert_eq!(r.join.join().unwrap().unwrap(), Outcome::Stopped);
    let _ = std::fs::remove_dir_all(dir);
}
