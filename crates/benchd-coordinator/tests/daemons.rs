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
