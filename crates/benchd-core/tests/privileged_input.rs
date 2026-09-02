//! What the privileged daemons must refuse.
//!
//! Two privilege escalations reached a root `mount(2)` because a value that
//! looked like a name or a path was never checked. These pin the validators
//! that now stand between agent- or network-supplied strings and the syscalls.

use benchd_core::model::{valid_component, valid_device_path};
use std::path::Path;

#[test]
fn only_real_device_paths_are_accepted_as_bench_resources() {
    // Anyone who can reach the coordinator can register a bench (§9 accepts
    // that). Without this check the declared path reaches `mount --bind` and
    // then `chown` in a root daemon, so registering a bench whose "device" is
    // /etc/shadow hands that file to an unprivileged agent.
    for hostile in [
        "/etc/shadow",
        "/root/.ssh/authorized_keys",
        "/etc/sudoers",
        "/dev/../etc/shadow",
        "dev/ttyUSB0",
        "../../dev/ttyUSB0",
        "/devious/ttyUSB0",
        "/",
    ] {
        assert!(
            valid_device_path(Path::new(hostile)).is_err(),
            "{hostile:?} must be refused: it reaches mount(2) as root"
        );
    }
}

#[test]
fn ordinary_device_paths_are_accepted() {
    for ok in [
        "/dev/ttyUSB0",
        "/dev/ttyACM0",
        "/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_30:ED:A0:EC:ED:41-if00",
    ] {
        assert!(valid_device_path(Path::new(ok)).is_ok(), "{ok:?} should be allowed");
    }
}

#[test]
fn path_components_that_could_escape_a_directory_are_refused() {
    // Slot names come from an agent's claim, resource names from a host's
    // config; both become directory components inside a root daemon.
    for hostile in ["..", ".", "a/b", "/abs", "", "..\\/..", "with space", "a\0b"] {
        assert!(!valid_component(hostile), "{hostile:?} must be refused");
    }
    for ok in ["dut", "peer", "node_a", "node-b", "usb.0"] {
        assert!(valid_component(ok), "{ok:?} should be allowed");
    }
}
