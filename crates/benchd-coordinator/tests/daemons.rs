//! Integration tests that drive the real daemons over their real sockets.
//!
//! Both privilege escalations found by the adversarial review lived here, in the
//! gap between unit-tested pure logic and hand-run happy-path checks. Nothing
//! automated had ever sent a hostile message to a running coordinator.
//!
//! These spawn a real `benchd coordinator` on a throwaway port with a temporary
//! inventory, and speak the wire protocol directly — the same way an attacker
//! would. They deliberately do *not* start `benchd client`, because that needs
//! root; the assertions here are about what the coordinator refuses to pass on.
//!
//! Ignored by default so `cargo test` stays hermetic and fast, and **serial**:
//!
//! ```sh
//! cargo test --test daemons -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1` is not optional. Each test spawns its own coordinator
//! process, and running eight debug builds at once starves them of CPU until
//! replies miss their deadline — a failure that looks like a protocol bug and is
//! not one. They bind an ephemeral port, so it is contention rather than a port
//! clash, but the effect is the same. CI passes the flag.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};

/// A coordinator on its own port with a minimal vocabulary.
struct Harness {
    coordinator: Child,
    port: u16,
    _dir: tempdir::TempDir,
}

mod tempdir {
    /// Minimal scratch directory that removes itself, to avoid a dev-dependency.
    pub struct TempDir(std::path::PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> std::io::Result<Self> {
            let path = std::env::temp_dir().join(format!(
                "benchd-it-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path)?;
            Ok(TempDir(path))
        }
        pub fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

const CONFIG: &str = r#"
open_keys = ["name"]

[limits]
max_ttl        = 900
max_total_hold = 7200
max_benches    = 2
grace          = 30

[tags.soc]
weight = 0
[tags.soc.values.esp32s3]
description = "ESP32-S3"
[tags.psram]
[tags.psram.values.none]
[tags.psram.values.octal]
"#;

/// The one binary everything now ships as.
fn binary() -> std::path::PathBuf {
    // target/<profile>/deps/<test binary> -> target/<profile>/benchd
    let mut dir = std::env::current_exe().expect("current_exe");
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let path = dir.join("benchd");
    assert!(
        path.exists(),
        "{} not built; run `cargo build` first",
        path.display()
    );
    path
}

impl Harness {
    fn start(tag: &str) -> Harness {
        Harness::start_with(tag, &[])
    }

    /// A coordinator with extra flags.
    ///
    /// The liveness reaper and the teardown deadline are both measured in tens
    /// of seconds by default, which is right in a lab and useless in a test.
    fn start_with(tag: &str, extra: &[&str]) -> Harness {
        let dir = tempdir::TempDir::new(tag).expect("temp dir");
        let config = dir.path().join("coordinator.toml");
        std::fs::write(&config, CONFIG).expect("write config");

        // Port 0, and let the kernel choose. Every test in this file starts its
        // own coordinator and they run in parallel, so a port derived from
        // anything the process shares — the pid, a constant — has them fighting
        // over one socket: the losers fail to bind and their tests then talk to
        // a coordinator belonging to some other test, or to nothing at all.
        // `--report-address` exists for this.
        let address = dir.path().join("address");
        let coordinator = Command::new(binary())
            .arg("coordinator")
            .arg("--config")
            .arg(&config)
            .arg("--listen")
            .arg("127.0.0.1:0")
            .arg("--report-address")
            .arg(&address)
            .args(extra)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn coordinator");

        let mut harness = Harness {
            coordinator,
            port: 0,
            _dir: dir,
        };

        // Wait for it to accept connections rather than sleeping blindly. The
        // file appears only once the listener is bound, and its contents are
        // written before the first accept.
        for _ in 0..100 {
            if let Some(port) = std::fs::read_to_string(&address)
                .ok()
                .and_then(|text| text.trim().rsplit(':').next()?.parse::<u16>().ok())
            {
                if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    harness.port = port;
                    return harness;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // Drop reaps the child, so the panic does not leak a coordinator.
        let _ = harness.coordinator.kill();
        let _ = harness.coordinator.wait();
        panic!("coordinator never reported a listening address");
    }

    /// Send one line and read one line back.
    fn exchange(&self, line: &str) -> String {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
        stream
            // Generous: CI machines are shared and a coordinator that has just
            // been spawned may be waiting on CPU. A real hang still fails, just
            // later.
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .expect("timeout");
        let mut writer = stream.try_clone().expect("clone");
        writeln!(writer, "{line}").expect("write");
        writer.flush().expect("flush");
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).expect("read");
        reply.trim().to_string()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.coordinator.kill();
        let _ = self.coordinator.wait();
    }
}

/// One connection to the coordinator, held open across a whole conversation.
///
/// The tests above open a socket per exchange. The ones at the bottom of this
/// file are about *who* is on the other end of a connection — which host
/// registered a bench, which peer an instruction was sent to, who is entitled
/// to report on it — so the connection has to persist to be the subject.
struct Peer {
    write: TcpStream,
    read: BufReader<TcpStream>,
}

/// What came back, or why nothing did. Silence and a closed socket mean very
/// different things here: hanging up is how the coordinator tells a host its
/// bench has been withdrawn.
#[derive(Debug)]
enum Incoming {
    Line(String),
    Closed,
    Silent,
}

impl Peer {
    fn connect(h: &Harness) -> Peer {
        let stream = TcpStream::connect(("127.0.0.1", h.port)).expect("connect");
        stream.set_nodelay(true).ok();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .expect("timeout");
        Peer {
            write: stream.try_clone().expect("clone"),
            read: BufReader::new(stream),
        }
    }

    /// Shorten the read timeout, for the assertions that want silence.
    fn expect_silence_within(&mut self, timeout: std::time::Duration) {
        self.read
            .get_ref()
            .set_read_timeout(Some(timeout))
            .expect("timeout");
    }

    fn send(&mut self, line: &str) {
        writeln!(self.write, "{line}").expect("write");
        self.write.flush().expect("flush");
    }

    fn next(&mut self) -> Incoming {
        let mut line = String::new();
        match self.read.read_line(&mut line) {
            Ok(0) => Incoming::Closed,
            Ok(_) => Incoming::Line(line.trim().to_string()),
            Err(_) => Incoming::Silent,
        }
    }

    fn recv(&mut self) -> String {
        match self.next() {
            Incoming::Line(line) => line,
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    fn open_session(&mut self, name: &str) -> String {
        self.send(&format!(
            r#"{{"msg":"open_session","request":1,"name":"{name}"}}"#
        ));
        let reply = self.recv();
        reply
            .split("\"session\":\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .unwrap_or_else(|| panic!("no session token in {reply}"))
            .to_string()
    }

    fn claim(&mut self, token: &str, request: u64, tags: &str) -> String {
        self.send(&format!(
            r#"{{"msg":"claim","request":{request},"session":"{token}","claim":{{"slots":{{"dut":["{tags}"]}},"ttl":60}}}}"#
        ));
        self.recv()
    }

    /// A round trip on this connection, so that everything sent before it has
    /// certainly been handled — and anything unsolicited that was queued
    /// behind it has been read.
    ///
    /// The coordinator serves each peer in its own task, so a message sent on
    /// one connection and a message sent on another are not ordered against
    /// each other. A test that assumes they are passes or fails on the
    /// scheduler.
    fn barrier(&mut self, token: &str) {
        self.send(&format!(
            r#"{{"msg":"status","request":9999,"session":"{token}"}}"#
        ));
        loop {
            if self.recv().contains("\"request\":9999") {
                return;
            }
        }
    }
}

/// A registration for a bench with one serial resource.
fn registration(id: &str) -> String {
    format!(
        r#"{{"msg":"register","bench":{{"id":"{id}","description":"","tags":["soc=esp32s3"],"resources":{{"console":{{"kind":"serial","path":"/dev/null"}}}}}}}}"#
    )
}

/// The `request` an instruction carries, so a reply can be correlated the way
/// a real executor correlates it.
fn request_of(line: &str) -> u64 {
    line.split("\"request\":")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|digits| digits.parse().ok())
        .unwrap_or_else(|| panic!("no request id in {line}"))
}

fn lease_of(line: &str) -> u64 {
    line.split("\"lease\":")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|digits| digits.parse().ok())
        .unwrap_or_else(|| panic!("no lease id in {line}"))
}

fn done_ok(request: u64) -> String {
    format!(r#"{{"msg":"done","request":{request},"result":{{"outcome":"ok"}}}}"#)
}

fn done_failed(request: u64) -> String {
    format!(
        r#"{{"msg":"done","request":{request},"result":{{"outcome":"failed","detail":"forged"}}}}"#
    )
}

/// A bench with a live lease on it: the host and the holder both connected,
/// both instructions delivered and neither answered yet.
///
/// Unanswered is deliberate. It is the state a lease is really in for most of
/// its setup — a client serialises every materialisation behind one mutex held
/// across a USB/IP import, so its `Done` can be tens of seconds away — and it
/// is the window in which an instruction can be answered by the wrong peer.
struct Leased {
    host: Peer,
    agent: Peer,
    token: String,
    lease: u64,
    export: u64,
    materialize: u64,
}

impl Leased {
    fn new(h: &Harness, bench: &str) -> Leased {
        let mut host = Peer::connect(h);
        host.send(&registration(bench));
        let reply = host.recv();
        assert!(reply.contains("registered"), "{reply}");

        let mut agent = Peer::connect(h);
        let token = agent.open_session("agent-1");
        let granted = agent.claim(&token, 2, "soc=esp32s3");
        assert!(granted.contains("granted"), "{granted}");
        let lease = lease_of(&granted);

        let materialize = agent.recv();
        assert!(
            materialize.contains("\"msg\":\"materialize\""),
            "{materialize}"
        );
        let export = host.recv();
        assert!(export.contains("\"msg\":\"export\""), "{export}");

        Leased {
            host,
            agent,
            token,
            lease,
            export: request_of(&export),
            materialize: request_of(&materialize),
        }
    }

    /// Both executors report success, as they would on a lease that came up.
    fn settle(&mut self) {
        let (export, materialize) = (self.export, self.materialize);
        self.host.send(&done_ok(export));
        self.agent.send(&done_ok(materialize));
    }

    fn release(&mut self) {
        let (token, lease) = (self.token.clone(), self.lease);
        self.agent.send(&format!(
            r#"{{"msg":"release","request":3,"session":"{token}","lease":{lease}}}"#
        ));
        let ok = self.agent.recv();
        assert!(ok.contains("\"msg\":\"ok\""), "{ok}");
    }
}

// --- hostile registrations -------------------------------------------------

#[test]
#[ignore = "spawns daemons"]
fn a_bench_claiming_a_device_outside_dev_is_refused() {
    // The escalation: a registration's device path reached mount(2) and chown(2)
    // in a root daemon. Registering a bench whose "device" is /etc/shadow handed
    // that file to an unprivileged agent, owned by them.
    let h = Harness::start("shadow");
    for hostile in [
        "/etc/shadow",
        "/root/.ssh/authorized_keys",
        "/dev/../etc/passwd",
        "relative/path",
    ] {
        let reply = h.exchange(&format!(
            r#"{{"msg":"register","bench":{{"id":"evil","description":"","tags":["soc=esp32s3"],"resources":{{"console":{{"kind":"serial","path":"{hostile}"}}}}}}}}"#
        ));
        assert!(
            reply.contains("rejected"),
            "registering {hostile:?} should be refused, got: {reply}"
        );
    }
}

#[test]
#[ignore = "spawns daemons"]
fn a_resource_or_bench_name_that_could_escape_a_directory_is_refused() {
    // These become path components inside the root client daemon.
    let h = Harness::start("names");

    let reply = h.exchange(
        r#"{"msg":"register","bench":{"id":"ok","description":"","tags":["soc=esp32s3"],"resources":{"../../etc":{"kind":"serial","path":"/dev/null"}}}}"#,
    );
    assert!(reply.contains("rejected"), "hostile resource name: {reply}");

    let reply = h.exchange(
        r#"{"msg":"register","bench":{"id":"../../etc","description":"","tags":["soc=esp32s3"],"resources":{"console":{"kind":"serial","path":"/dev/null"}}}}"#,
    );
    assert!(reply.contains("rejected"), "hostile bench id: {reply}");
}

#[test]
#[ignore = "spawns daemons"]
fn a_bench_with_an_unknown_tag_is_refused_with_a_suggestion() {
    let h = Harness::start("tags");
    let reply = h.exchange(
        r#"{"msg":"register","bench":{"id":"typo","description":"","tags":["soc=esp32s4"],"resources":{"console":{"kind":"serial","path":"/dev/null"}}}}"#,
    );
    assert!(reply.contains("rejected"), "{reply}");
    assert!(
        reply.contains("esp32s3"),
        "the refusal should suggest the intended tag: {reply}"
    );
}

#[test]
#[ignore = "spawns daemons"]
fn a_failed_registration_does_not_destroy_the_bench_already_there() {
    // Validation used to run *after* the incumbent was evicted, so a stale
    // config on another machine deleted a healthy bench and killed its leases,
    // then failed — and the healthy host never re-registered.
    let h = Harness::start("evict");

    // A good host, held open so its registration stays live.
    let good = TcpStream::connect(("127.0.0.1", h.port)).expect("connect");
    let mut w = good.try_clone().expect("clone");
    writeln!(
        w,
        r#"{{"msg":"register","bench":{{"id":"shared","description":"","tags":["soc=esp32s3"],"resources":{{"console":{{"kind":"serial","path":"/dev/null"}}}}}}}}"#
    )
    .expect("write");
    w.flush().expect("flush");
    let mut reply = String::new();
    BufReader::new(good.try_clone().unwrap())
        .read_line(&mut reply)
        .expect("read");
    assert!(reply.contains("registered"), "good host: {reply}");

    // Now a bad registration for the same bench id.
    let bad = h.exchange(
        r#"{"msg":"register","bench":{"id":"shared","description":"","tags":["soc=nonsense"],"resources":{"console":{"kind":"serial","path":"/dev/null"}}}}"#,
    );
    assert!(
        reply.contains("registered") && bad.contains("rejected"),
        "bad host: {bad}"
    );

    // The good bench must still be listed.
    let state = h.exchange(r#"{"msg":"inspect"}"#);
    assert!(
        state.contains("\"shared\""),
        "the healthy bench was destroyed by a failed registration: {state}"
    );
}

#[test]
#[ignore = "spawns daemons"]
fn a_host_cannot_push_a_datasheet_into_every_agents_context() {
    let h = Harness::start("bigdocs");
    let docs = "x".repeat(9000);
    let reply = h.exchange(&format!(
        r#"{{"msg":"register","bench":{{"id":"verbose","description":"","docs":"{docs}","tags":["soc=esp32s3"],"resources":{{"console":{{"kind":"serial","path":"/dev/null"}}}}}}}}"#
    ));
    assert!(reply.contains("rejected"), "{reply}");
    assert!(reply.contains("over the"), "{reply}");
}

#[test]
#[ignore = "spawns daemons"]
fn two_resources_on_one_device_share_a_single_channel() {
    // A USB-SD-Mux is switched through its SCSI node and written through its
    // block node, but USB/IP forwards whole devices — so one busid must produce
    // one export. Minting a channel per resource left the second with nobody to
    // pair with in the relay, and the client waited out its timeout on an import
    // the host was never asked to make.
    let h = Harness::start("shared-busid");

    let host = TcpStream::connect(("127.0.0.1", h.port)).expect("connect");
    host.set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .unwrap();
    let mut hw = host.try_clone().expect("clone");
    let mut hr = BufReader::new(host);
    writeln!(
        hw,
        r#"{{"msg":"register","bench":{{"id":"muxed","description":"","tags":["soc=esp32s3"],"resources":{{"sdmux":{{"kind":"usb","busid":"3-1.1","node":"scsi"}},"sdcard":{{"kind":"usb","busid":"3-1.1","node":"block"}}}}}}}}"#
    )
    .expect("write");
    hw.flush().expect("flush");
    let mut reply = String::new();
    hr.read_line(&mut reply).expect("read");
    assert!(reply.contains("registered"), "{reply}");

    let stream = TcpStream::connect(("127.0.0.1", h.port)).expect("connect");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .unwrap();
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);

    writeln!(
        w,
        r#"{{"msg":"open_session","request":1,"name":"agent-1"}}"#
    )
    .unwrap();
    w.flush().unwrap();
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    let token = line
        .split("\"session\":\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("token")
        .to_string();

    line.clear();
    writeln!(
        w,
        r#"{{"msg":"claim","request":2,"session":"{token}","claim":{{"slots":{{"dut":["soc=esp32s3"]}},"ttl":60}}}}"#
    )
    .unwrap();
    w.flush().unwrap();
    r.read_line(&mut line).unwrap();
    assert!(line.contains("granted"), "{line}");

    // The export the host is told to perform.
    let mut export = String::new();
    hr.read_line(&mut export).expect("export");
    assert!(export.contains("\"msg\":\"export\""), "{export}");

    let channels: Vec<&str> = ["sdmux", "sdcard"]
        .iter()
        .map(|name| {
            export
                .split(&format!("\"{name}\":\""))
                .nth(1)
                .and_then(|s| s.split('"').next())
                .unwrap_or_else(|| panic!("no channel for {name} in {export}"))
        })
        .collect();
    assert_eq!(
        channels[0], channels[1],
        "both nodes of one device must ride one channel: {export}"
    );
}

// --- the agent-facing surface ----------------------------------------------

#[test]
#[ignore = "spawns daemons"]
fn a_grant_carries_the_benchs_wiring_notes() {
    // The pinout is only knowable from the bench config, and only the holder of
    // the lease is entitled to it, so grant time is the one place it can be
    // delivered.
    let h = Harness::start("docs");

    let host = TcpStream::connect(("127.0.0.1", h.port)).expect("connect");
    let mut hw = host.try_clone().expect("clone");
    writeln!(
        hw,
        r#"{{"msg":"register","bench":{{"id":"documented","description":"","docs":"GPIO4 -> LED","tags":["soc=esp32s3"],"resources":{{"console":{{"kind":"serial","path":"/dev/null"}}}}}}}}"#
    )
    .expect("write");
    hw.flush().expect("flush");
    let mut reply = String::new();
    BufReader::new(host.try_clone().unwrap())
        .read_line(&mut reply)
        .expect("read");
    assert!(reply.contains("registered"), "{reply}");

    let stream = TcpStream::connect(("127.0.0.1", h.port)).expect("connect");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .unwrap();
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);

    writeln!(
        w,
        r#"{{"msg":"open_session","request":1,"name":"agent-1"}}"#
    )
    .unwrap();
    w.flush().unwrap();
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    let token = line
        .split("\"session\":\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("token")
        .to_string();

    line.clear();
    writeln!(
        w,
        r#"{{"msg":"claim","request":2,"session":"{token}","claim":{{"slots":{{"dut":["soc=esp32s3"]}},"ttl":60}}}}"#
    )
    .unwrap();
    w.flush().unwrap();
    r.read_line(&mut line).unwrap();
    assert!(line.contains("granted"), "{line}");
    assert!(
        line.contains("GPIO4 -> LED"),
        "the grant must carry the bench's notes: {line}"
    );
}

#[test]
#[ignore = "spawns daemons"]
fn an_agent_cannot_claim_a_bench_by_name() {
    // D17: agents describe capabilities; naming a bench is the operator CLI's
    // job. Otherwise one hardcodes a bench into a script and reintroduces the
    // contention this system removes.
    let h = Harness::start("byname");
    let session = h.exchange(r#"{"msg":"open_session","request":1,"name":"agent-1"}"#);
    let token = session
        .split("\"session\":\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("token")
        .to_string();

    // Same connection, so the session is still known.
    let stream = TcpStream::connect(("127.0.0.1", h.port)).expect("connect");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .unwrap();
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);

    writeln!(
        w,
        r#"{{"msg":"open_session","request":1,"name":"agent-1"}}"#
    )
    .unwrap();
    w.flush().unwrap();
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    let token2 = line
        .split("\"session\":\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .unwrap_or(&token)
        .to_string();

    line.clear();
    writeln!(
        w,
        r#"{{"msg":"claim","request":2,"session":"{token2}","claim":{{"slots":{{"dut":["name=anything"]}},"ttl":60}}}}"#
    )
    .unwrap();
    w.flush().unwrap();
    r.read_line(&mut line).unwrap();
    assert!(line.contains("error"), "{line}");
    assert!(
        line.contains("by name is not allowed"),
        "claiming by name must be refused: {line}"
    );
}

#[test]
#[ignore = "spawns daemons"]
fn a_claim_with_a_hostile_slot_name_is_refused() {
    // Slot names are arbitrary JSON keys from an agent and become directory
    // components in a root daemon.
    let h = Harness::start("slot");
    let stream = TcpStream::connect(("127.0.0.1", h.port)).expect("connect");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .unwrap();
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);

    writeln!(
        w,
        r#"{{"msg":"open_session","request":1,"name":"agent-1"}}"#
    )
    .unwrap();
    w.flush().unwrap();
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    let token = line
        .split("\"session\":\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("token")
        .to_string();

    for hostile in ["../../../../tmp/pwned", "/dev", ".."] {
        line.clear();
        writeln!(
            w,
            r#"{{"msg":"claim","request":3,"session":"{token}","claim":{{"slots":{{"{hostile}":["soc=esp32s3"]}},"ttl":60}}}}"#
        )
        .unwrap();
        w.flush().unwrap();
        r.read_line(&mut line).unwrap();
        assert!(
            line.contains("invalid slot name"),
            "slot {hostile:?} must be refused: {line}"
        );
    }
}

#[test]
#[ignore = "spawns daemons"]
fn an_unrecognised_first_message_gets_an_error_rather_than_a_hang() {
    // A version mismatch used to fall through to the client path and be silently
    // ignored, leaving the caller waiting forever.
    let h = Harness::start("garbage");
    let reply = h.exchange(r#"{"msg":"nonsense","request":1}"#);
    assert!(reply.contains("error"), "{reply}");
    assert!(
        reply.contains("same build"),
        "the error should point at a version mismatch: {reply}"
    );
}

#[test]
#[ignore = "spawns daemons"]
fn prepare_owner_is_refused_by_the_coordinator() {
    // It is a client-daemon request; the coordinator has no idea where this
    // machine puts device nodes and must not pretend otherwise.
    let h = Harness::start("prepare");
    let reply = h.exchange(r#"{"msg":"prepare_owner","request":1,"name":"x"}"#);
    assert!(reply.contains("error"), "{reply}");
    assert!(reply.contains("client-daemon"), "{reply}");
}

// --- who is allowed to report on an instruction ----------------------------

#[test]
#[ignore = "spawns daemons"]
fn only_the_peer_an_instruction_was_sent_to_may_report_on_it() {
    // Request ids come from one counter shared between host exports and client
    // materialisations and start at 1, and the pending table recorded only the
    // lease. So a connection that had never opened a session and held no token
    // could say `{"msg":"done","request":1,...}` and have lease 1 dropped under
    // its holder — the holder told `failed` and `ended`, the host told to
    // unexport — and a sweep of the first twenty ids cleared the lab.
    let h = Harness::start("forged-done");
    let mut held = Leased::new(&h, "board");

    // A stranger walking the low request ids, exactly as the reproduction did.
    // It has sent nothing else, so the coordinator knows it only as a client
    // connection with no session at all.
    let mut stranger = Peer::connect(&h);
    for request in 1..=20 {
        stranger.send(&done_failed(request));
    }
    // A round trip of its own, so the sweep is known to have been handled
    // rather than merely sent.
    stranger.open_session("stranger");

    // The holder must hear nothing, and the host must not be told to tear down.
    held.agent
        .expect_silence_within(std::time::Duration::from_secs(2));
    if let Incoming::Line(line) = held.agent.next() {
        panic!("a forged report reached the holder: {line}");
    }
    held.host
        .expect_silence_within(std::time::Duration::from_secs(2));
    if let Incoming::Line(line) = held.host.next() {
        panic!("a forged report had the host tear its bench down: {line}");
    }

    let state = h.exchange(r#"{"msg":"inspect"}"#);
    assert!(
        !state.contains("\"leases\":[]"),
        "a forged report destroyed the lease: {state}"
    );
}

#[test]
#[ignore = "spawns daemons"]
fn a_forged_success_cannot_hide_a_real_failure() {
    // The other half of the same hole. A forged `ok` used to take the pending
    // entry with it, so when the real executor reported that it could not carry
    // the instruction out, the coordinator no longer knew which lease that was
    // about — and the agent kept a lease whose hardware was never exported.
    let h = Harness::start("forged-ok");
    let mut held = Leased::new(&h, "board");

    let mut stranger = Peer::connect(&h);
    stranger.send(&done_ok(held.export));
    // A round trip on the stranger's own connection, so the forged report is
    // known to have been handled before the real one is sent. Otherwise the
    // test races the two and passes for the wrong reason.
    stranger.open_session("stranger");

    let export = held.export;
    held.host.send(&format!(
        r#"{{"msg":"done","request":{export},"result":{{"outcome":"failed","detail":"usbip bind failed"}}}}"#
    ));

    let failed = held.agent.recv();
    assert!(
        failed.contains("\"msg\":\"failed\""),
        "the holder must still be told its lease never came up: {failed}"
    );
    assert!(failed.contains("usbip bind failed"), "{failed}");
}

#[test]
#[ignore = "spawns daemons"]
fn a_host_cannot_answer_for_a_client_nor_a_client_for_a_host() {
    // The same check from the other direction: both peers here are real
    // executors holding real connections, and each answers the instruction the
    // other was given.
    let h = Harness::start("crossed-done");
    let mut held = Leased::new(&h, "board");

    let (export, materialize) = (held.export, held.materialize);
    held.host.send(&done_failed(materialize));
    held.agent.send(&done_failed(export));

    held.agent
        .expect_silence_within(std::time::Duration::from_secs(2));
    if let Incoming::Line(line) = held.agent.next() {
        panic!("an instruction answered by the wrong peer ended the lease: {line}");
    }
    let state = h.exchange(r#"{"msg":"inspect"}"#);
    assert!(!state.contains("\"leases\":[]"), "{state}");
}

// --- a bench that comes back -----------------------------------------------

#[test]
#[ignore = "spawns daemons"]
fn a_host_gets_a_withdrawn_bench_back_by_registering_again() {
    // There was no path back from a withdrawn bench. Heartbeats found no entry
    // to touch, and a second `register` on a live connection was discarded — so
    // a host that lost its bench stayed connected, heartbeating and physically
    // holding the hardware, with no way to offer it again.
    let h = Harness::start("reregister");

    let mut host = Peer::connect(&h);
    host.send(&registration("board"));
    assert!(host.recv().contains("registered"));

    // Withdraw the bench without disturbing the connection.
    host.send(r#"{"msg":"device_lost","resource":"console","detail":"unplugged"}"#);
    let mut withdrawn = false;
    for _ in 0..100 {
        if !h.exchange(r#"{"msg":"inspect"}"#).contains("\"board\"") {
            withdrawn = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(withdrawn, "the bench should have been withdrawn");

    host.send(&registration("board"));
    let reply = host.recv();
    assert!(
        reply.contains("registered"),
        "a repeat registration must be answered, not discarded: {reply}"
    );

    // Listed again, and claimable again rather than merely listed.
    let state = h.exchange(r#"{"msg":"inspect"}"#);
    assert!(state.contains("\"board\""), "{state}");

    let mut agent = Peer::connect(&h);
    let token = agent.open_session("agent-1");
    let granted = agent.claim(&token, 2, "soc=esp32s3");
    assert!(granted.contains("granted"), "{granted}");
}

#[test]
#[ignore = "spawns daemons"]
fn a_host_declared_dead_is_told_to_unexport_and_then_hung_up_on() {
    // Two failures in one path. The reaper forgot the route to the host before
    // dispatching the teardown, so `Unexport` found nobody and was skipped:
    // harmless for a host that really is dead, but one wrongly declared dead is
    // alive on an open socket and was never told, and kept its device
    // stub-bound and its relay socket pumping for a lease the coordinator had
    // forgotten while the client was told to unmaterialise. And nothing closed
    // the connection either, so a host that had merely been slow had no route
    // back at all.
    let h = Harness::start_with("declared-dead", &["--host-timeout-seconds", "8"]);
    let mut held = Leased::new(&h, "board");
    held.settle();

    // No heartbeats from here on: the reaper will decide this host is gone.
    let unexport = held.host.recv();
    assert!(
        unexport.contains("\"msg\":\"unexport\""),
        "a host declared dead must still be told to let go: {unexport}"
    );
    assert!(
        matches!(held.host.next(), Incoming::Closed),
        "and then hung up on, because nothing else can tell it its bench is gone"
    );

    // The holder learns about it too, as it always did.
    let unmaterialize = held.agent.recv();
    assert!(
        unmaterialize.contains("\"msg\":\"unmaterialize\""),
        "{unmaterialize}"
    );
}

#[test]
#[ignore = "spawns daemons"]
fn a_disconnecting_host_does_not_take_a_bench_another_one_still_serves() {
    // A second host may claim a bench id already held, by design, so a
    // restarted host can take its own bench back. That left the first host
    // connected and still physically holding the hardware but forgotten — so
    // when the second disconnected, the bench went with it and never came back.
    // No race and no timeout involved.
    let h = Harness::start("standby");

    let mut first = Peer::connect(&h);
    first.send(&registration("board"));
    assert!(first.recv().contains("registered"));

    let mut second = Peer::connect(&h);
    second.send(&registration("board"));
    assert!(second.recv().contains("registered"));

    // A lease on the bench, so the disconnect below has an observable
    // consequence to wait for rather than a sleep to guess at.
    let mut agent = Peer::connect(&h);
    let token = agent.open_session("agent-1");
    let granted = agent.claim(&token, 2, "soc=esp32s3");
    assert!(granted.contains("granted"), "{granted}");
    let materialize = agent.recv();
    agent.send(&done_ok(request_of(&materialize)));
    let export = second.recv();
    assert!(export.contains("\"msg\":\"export\""), "{export}");

    drop(second);

    // The lease dies with the connection that was exporting it; that is the
    // signal the disconnect has been processed.
    let unmaterialize = agent.recv();
    assert!(
        unmaterialize.contains("\"msg\":\"unmaterialize\""),
        "{unmaterialize}"
    );
    agent.send(&done_ok(request_of(&unmaterialize)));
    let ended = agent.recv();
    assert!(ended.contains("\"msg\":\"ended\""), "{ended}");

    let state = h.exchange(r#"{"msg":"inspect"}"#);
    assert!(
        state.contains("\"board\""),
        "the bench left with a host that was not the only one serving it: {state}"
    );

    // And it is the host that is still there which gets the next export.
    let granted = agent.claim(&token, 3, "soc=esp32s3");
    assert!(granted.contains("granted"), "{granted}");
    let export = first.recv();
    assert!(
        export.contains("\"msg\":\"export\""),
        "the export must reach the host that is still connected: {export}"
    );
}

// --- letting go before handing on -------------------------------------------

#[test]
#[ignore = "spawns daemons"]
fn a_bench_is_not_handed_on_until_its_holder_has_let_go() {
    // "Unmaterialise before unexport" was only ever an ordering of *sends*. The
    // lease left the lease table before either instruction went out, so the
    // bench left the busy set at the same moment and the next claim could be
    // granted it — and the host told to export it — while the previous holder
    // was still inside an unmaterialisation it serialises behind one global
    // mutex held across a USB/IP operation.
    let h = Harness::start("drain");
    let mut held = Leased::new(&h, "board");
    held.settle();
    held.release();

    let unmaterialize = held.agent.recv();
    assert!(
        unmaterialize.contains("\"msg\":\"unmaterialize\""),
        "{unmaterialize}"
    );
    let unexport = held.host.recv();
    assert!(unexport.contains("\"msg\":\"unexport\""), "{unexport}");

    // Deliberately not acknowledged yet: this is the window.
    let mut second = Peer::connect(&h);
    let other = second.open_session("agent-2");
    let refused = second.claim(&other, 2, "soc=esp32s3");
    assert!(
        refused.contains("\"msg\":\"error\""),
        "the bench was handed on before its holder had let go: {refused}"
    );
    assert!(
        refused.contains("still being released"),
        "the refusal should say why: {refused}"
    );
    assert!(
        refused.contains("\"retryable\":true"),
        "a bench that is seconds away must not be reported as one that will \
         never exist: {refused}"
    );

    // Now it has let go.
    held.agent.send(&done_ok(request_of(&unmaterialize)));
    held.agent.barrier(&held.token);
    let granted = second.claim(&other, 3, "soc=esp32s3");
    assert!(
        granted.contains("granted"),
        "the bench should be allocatable the moment its holder answers: {granted}"
    );
}

#[test]
#[ignore = "spawns daemons"]
fn a_holder_that_never_answers_does_not_strand_the_bench() {
    // The bounded escape. A bench waiting for a `Done` that will never come is
    // a worse bug than the stale mount the wait exists to prevent, and a client
    // can die mid-teardown or fail the unmaterialisation outright.
    let h = Harness::start_with("drain-timeout", &["--teardown-ack-seconds", "2"]);
    let mut held = Leased::new(&h, "board");
    held.settle();
    held.release();

    let unmaterialize = held.agent.recv();
    assert!(
        unmaterialize.contains("\"msg\":\"unmaterialize\""),
        "{unmaterialize}"
    );

    // Never answered.
    let mut second = Peer::connect(&h);
    let other = second.open_session("agent-2");
    let refused = second.claim(&other, 2, "soc=esp32s3");
    assert!(
        refused.contains("still being released"),
        "the hold should be in force to begin with, or this proves nothing: {refused}"
    );

    for _ in 0..40 {
        if second.claim(&other, 3, "soc=esp32s3").contains("granted") {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    panic!("the bench was still waiting for an acknowledgement that will never come");
}
