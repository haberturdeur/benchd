//! Handing a connected socket to the kernel's USB/IP drivers.
//!
//! Both drivers take an **already-connected fd**, which is what lets benchd dial
//! outward and never listen (D5). After the write, the kernel owns the
//! connection: `sockfd_lookup` takes its own reference, so the socket outlives
//! the process that created it.
//!
//! That last point is why teardown is mandatory rather than tidy. A host that
//! exits without writing `-1` leaves the kernel pumping a socket for a lease
//! that no longer exists — hardware reachable by an agent whose claim is gone,
//! which is the exact failure the whole system exists to prevent (D6).

use std::io;
use std::os::fd::{AsRawFd, IntoRawFd};
use std::path::{Path, PathBuf};

use tokio::net::TcpStream;

use crate::wire::WantedNode;

const STUB: &str = "/sys/bus/usb/drivers/usbip-host";
const VHCI: &str = "/sys/devices/platform/vhci_hcd.0";

/// Longest we will wait for a single sysfs write.
///
/// These writes normally return in microseconds, but a wedged usbip driver can
/// block one *forever* — tearing down a stub whose peer socket is in a bad state
/// waits on kernel threads that never finish. Without a bound, a host hangs at
/// startup with no output at all and systemd cheerfully reports it as active.
/// A stuck driver is not something userspace can fix, so the only useful
/// behaviour is to say so and carry on.
const SYSFS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Write to sysfs, giving up rather than blocking forever.
async fn write_sysfs(path: impl AsRef<std::path::Path>, value: &str) -> io::Result<()> {
    let path = path.as_ref().to_path_buf();
    match tokio::time::timeout(SYSFS_TIMEOUT, tokio::fs::write(&path, value)).await {
        Ok(result) => result,
        Err(_) => {
            tracing::error!(
                path = %path.display(), value,
                "sysfs write timed out; the usbip driver is wedged and needs a module \
                 reload or a reboot"
            );
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "sysfs write timed out",
            ))
        }
    }
}

// -- host side --------------------------------------------------------------

pub fn stub_path(busid: &str) -> PathBuf {
    PathBuf::from(STUB).join(busid)
}

pub fn is_bound(busid: &str) -> bool {
    stub_path(busid).exists()
}

/// Detach a device from its normal driver and bind the USB/IP stub to it.
///
/// Order matters: the stub refuses a device another driver still holds.
pub async fn bind(busid: &str) -> io::Result<()> {
    if is_bound(busid) {
        return Ok(());
    }
    let _ = write_sysfs(format!("/sys/bus/usb/devices/{busid}/driver/unbind"), busid).await;
    let _ = write_sysfs(format!("{STUB}/match_busid"), &format!("add {busid}")).await;
    write_sysfs(format!("{STUB}/bind"), busid).await
}

pub async fn unbind(busid: &str) {
    // Stop the kernel pumping first, then release the device. The reverse order
    // leaves a live socket attached to a device we no longer own.
    let _ = write_sysfs(stub_path(busid).join("usbip_sockfd"), "-1").await;
    let _ = write_sysfs(format!("{STUB}/unbind"), busid).await;
    // Dropped even when the board is gone: the entry is what makes the stub
    // claim a device the moment it appears, so leaving it behind means a
    // replugged board comes back with no tty and cannot be resolved.
    let _ = write_sysfs(format!("{STUB}/match_busid"), &format!("del {busid}")).await;
    // Nothing to probe for a device that is no longer plugged in, and
    // `reattach` would spend its whole retry budget failing before logging an
    // error about a board that is simply absent.
    if tokio::fs::metadata(format!("/sys/bus/usb/devices/{busid}"))
        .await
        .is_ok()
    {
        reattach(busid).await;
    }
}

/// Put a device back under its normal driver.
///
/// Worth retrying: immediately after `unbind` the kernel has often not finished
/// tearing the stub down, and a single `drivers_probe` silently does nothing.
/// The device is then left with no driver at all — physically present, no tty,
/// invisible to everything that looks it up by `/dev/serial/by-id`. A board in
/// that state stays dead until someone finds it by hand, so it is worth several
/// seconds of patience here.
pub async fn reattach(busid: &str) {
    for attempt in 0..6 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
        let _ = write_sysfs("/sys/bus/usb/drivers_probe", busid).await;
        if has_driver(busid).await {
            if attempt > 0 {
                tracing::info!(%busid, attempt, "device returned to its normal driver");
            }
            return;
        }
    }
    tracing::error!(
        %busid,
        "device is left with no driver; it will have no tty until it is re-probed \
         or replugged"
    );
}

/// Whether any interface of this device has a driver bound.
async fn has_driver(busid: &str) -> bool {
    let Ok(mut entries) = tokio::fs::read_dir(format!("/sys/bus/usb/devices/{busid}")).await else {
        return false;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Interfaces are named like `3-2:1.0`.
        if !name.starts_with(busid) || !name.contains(':') {
            continue;
        }
        if tokio::fs::metadata(entry.path().join("driver"))
            .await
            .is_ok()
        {
            return true;
        }
    }
    false
}

/// Whether this device currently presents a serial port.
pub async fn has_tty(busid: &str) -> bool {
    tty_under(
        &PathBuf::from(format!("/sys/bus/usb/devices/{busid}")),
        None,
    )
    .await
    .is_some()
}

/// Every USB device on the system, as `(busid, serial)`.
pub async fn devices_by_serial() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir("/sys/bus/usb/devices").await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let busid = entry.file_name().to_string_lossy().into_owned();
        if !busid.starts_with(|c: char| c.is_ascii_digit()) || busid.contains(':') {
            continue;
        }
        if let Ok(serial) = tokio::fs::read_to_string(entry.path().join("serial")).await {
            let serial = serial.trim().to_string();
            if !serial.is_empty() {
                out.push((busid, serial));
            }
        }
    }
    out
}

/// The sysfs directory of the USB device behind a `/dev/tty*` node.
///
/// The depth is not fixed. A CDC-ACM tty hangs off the interface, one level
/// below the device; a USB-serial bridge inserts a `usb-serial` port node, so
/// the device is two levels up. Assuming one level resolved ESP32s correctly
/// and returned nothing for CP2102s — which made the liveness watcher fall back
/// to polling the tty, and a tty vanishes the moment the bench is exported for
/// a remote lease, so every relayed lease on such a bench was torn down within
/// seconds as "device lost". `busnum` is the marker that says we have arrived:
/// interfaces and port nodes do not carry it, the device does.
pub fn usb_device_of_tty(tty: &Path) -> Option<PathBuf> {
    let name = tty.file_name()?.to_str()?;
    let mut dir = std::fs::canonicalize(format!("/sys/class/tty/{name}/device")).ok()?;
    for _ in 0..6 {
        if dir.join("busnum").exists() {
            return Some(dir);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

/// Which USB interface a `/dev/tty*` node hangs off.
///
/// A device with one serial port does not need this. A device with two does:
/// on an FT2232H the channels are interfaces 0 and 1, and on a WROVER-KIT one
/// of them is the JTAG channel and the other is the console. The host records
/// this while the tty still exists, so that after a USB/IP import the client
/// can pick the same port again instead of whichever the kernel lists first.
///
/// `bInterfaceNumber` is hex-formatted in sysfs, so interface 10 reads `0a`.
pub fn usb_interface_of_tty(tty: &Path) -> Option<u8> {
    let name = tty.file_name()?.to_str()?;
    let mut dir = std::fs::canonicalize(format!("/sys/class/tty/{name}/device")).ok()?;
    for _ in 0..6 {
        if let Ok(text) = std::fs::read_to_string(dir.join("bInterfaceNumber")) {
            return u8::from_str_radix(text.trim(), 16).ok();
        }
        // `busnum` marks the device itself: we have walked past every
        // interface without finding one, so there is nothing to report.
        if dir.join("busnum").exists() {
            return None;
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

/// Whether a device directory belongs to hardware forwarded in over USB/IP
/// rather than to something plugged into this machine.
///
/// A forwarded device is a faithful copy: same vendor, same product, same
/// serial, and so the same `/dev/serial/by-id` name as the board it came from.
/// Harmless when it lands on another machine, dangerous when it lands on this
/// one — a host whose bench is imported back over loopback can resolve its own
/// copy instead of the board, and would then hide a phantom while leaving the
/// real device on its driver, visible to everything the hiding exists to keep
/// it from. Benches declared `by_path` never hit this; a virtual device has a
/// different physical path.
///
/// Matched on the path component rather than against the full `vhci_hcd.0`
/// path, because a second vhci instance is `vhci_hcd.1` and failing to
/// recognise one means stubbing a device that is not really there.
pub fn is_forwarded(dir: &Path) -> bool {
    dir.components().any(|part| {
        part.as_os_str()
            .to_str()
            .is_some_and(|s| s.starts_with("vhci_hcd"))
    })
}

/// [`is_forwarded`], for a device already known by bus id.
pub fn is_forwarded_busid(busid: &str) -> bool {
    std::fs::canonicalize(format!("/sys/bus/usb/devices/{busid}"))
        .as_deref()
        .map(is_forwarded)
        .unwrap_or(false)
}

/// Hand a connected socket to the stub driver for `busid`.
///
/// Consumes the stream: the fd is deliberately leaked into the kernel's care,
/// because dropping it here would close the connection the kernel is about to
/// use. Teardown happens through [`unbind`], not through Rust's `Drop`.
pub async fn stub_attach(busid: &str, stream: TcpStream) -> io::Result<()> {
    let std_stream = stream.into_std()?;
    std_stream.set_nonblocking(false)?;
    let fd = std_stream.as_raw_fd();

    let result = write_sysfs(stub_path(busid).join("usbip_sockfd"), &fd.to_string()).await;

    // The kernel took its own reference on success, so our copy could be closed;
    // we leak it anyway rather than risk a race between close and the kernel's
    // lookup. One fd per active lease is not a leak worth chasing.
    let _ = std_stream.into_raw_fd();
    result
}

// -- client side ------------------------------------------------------------

/// A free virtual port on the vhci hub that matches a device's speed.
///
/// `status` lists **both** root hubs in one table:
///
/// ```text
/// hub port sta spd dev      sockfd local_busid
/// hs  0000 006 002 0007003b 000012 9-1
/// ss  0008 004 000 00000000 000000 0-0
/// ```
///
/// where `sta` 4 is `VDEV_ST_NULL` (free) and 6 is `VDEV_ST_USED`. The `hub`
/// column is not decoration: high-speed ports and SuperSpeed ports are separate
/// ranges, and attaching a full-speed device to an SS port fails with `EBUSY`.
/// Ignoring it worked until enough ports were occupied for the search to run off
/// the end of the hs range — so a single device always succeeded and a second
/// one sometimes did not.
pub async fn free_vhci_port(speed: u32) -> io::Result<u32> {
    let want = if speed >= 5 { "ss" } else { "hs" };
    let status = tokio::fs::read_to_string(format!("{VHCI}/status")).await?;

    for line in status.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let (Some(hub), Some(port), Some(state)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if hub != want {
            continue;
        }
        if parse_padded(state) != Some(4) {
            continue;
        }
        if let Some(port) = parse_padded(port) {
            return Ok(port);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        format!("no free {want} vhci port; every virtual slot for this device speed is in use"),
    ))
}

/// Parse a zero-padded sysfs field such as `0008` or `004`.
fn parse_padded(field: &str) -> Option<u32> {
    let trimmed = field.trim_start_matches('0');
    if trimmed.is_empty() {
        field.chars().all(|c| c == '0').then_some(0)
    } else {
        trimmed.parse().ok()
    }
}

/// Attach a connected socket to the local vhci hub, importing the device.
///
/// Consumes the stream for the same reason as [`stub_attach`].
pub async fn vhci_attach(port: u32, stream: TcpStream, devid: u32, speed: u32) -> io::Result<()> {
    let std_stream = stream.into_std()?;
    std_stream.set_nonblocking(false)?;
    let fd = std_stream.as_raw_fd();

    let result = write_sysfs(
        format!("{VHCI}/attach"),
        &format!("{port} {fd} {devid} {speed}"),
    )
    .await;

    let _ = std_stream.into_raw_fd();
    result
}

pub async fn vhci_detach(port: u32) {
    let _ = write_sysfs(format!("{VHCI}/detach"), &port.to_string()).await;
}

/// Which ports this machine currently has attached, and to what.
///
/// Used at startup to clear anything a previous incarnation left behind.
pub async fn attached_ports() -> Vec<u32> {
    let Ok(status) = tokio::fs::read_to_string(format!("{VHCI}/status")).await else {
        return Vec::new();
    };
    status
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _hub = fields.next()?;
            let port = fields.next()?;
            let state = fields.next()?;
            // 6 is VDEV_ST_USED.
            (parse_padded(state)? == 6).then_some(parse_padded(port)?)
        })
        .collect()
}

/// Wait for a device node that a freshly imported device produces, on `port`.
///
/// The kernel enumerates asynchronously, so nothing exists the instant `attach`
/// returns.
///
/// **Found by sysfs position, not by name.** A by-id name is built from vendor,
/// product and serial number, so a device imported from another machine can
/// produce *exactly* the name of one that just disappeared locally — which is
/// what happens when a bench is forwarded back to the machine it lives on.
/// Names are worse still for storage: the client machine very likely has a
/// `/dev/sda` of its own. The port tells the truth.
///
/// vhci exposes two root hubs, high speed and super speed. A device on rhport
/// `p` of a hub whose bus is `b` enumerates as `b-(p+1)`.
pub async fn wait_for_vhci_node(
    port: u32,
    speed: u32,
    want: WantedNode,
    timeout: std::time::Duration,
) -> Option<PathBuf> {
    let super_speed = speed >= 5;
    let deadline = tokio::time::Instant::now() + timeout;

    while tokio::time::Instant::now() < deadline {
        if let Some(bus) = vhci_bus(super_speed).await {
            let device = PathBuf::from(VHCI)
                .join(format!("usb{bus}"))
                .join(format!("{bus}-{}", port + 1));
            if let Some(name) = node_under(&device, want).await {
                // sysfs gains the node before udev creates it under /dev, so a
                // name with nothing behind it yet just means "not ready"; the
                // loop comes back for it.
                let path = PathBuf::from("/dev").join(name);
                if tokio::fs::metadata(&path).await.is_ok() {
                    // Let udev finish applying permissions before anyone opens it.
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    return Some(path);
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    None
}

/// The USB bus number of one of vhci's root hubs.
///
/// Note the strict `usb<digits>` match: `vhci_hcd.0` also contains a file called
/// `usbip_debug`, and a looser prefix test picks it up. An early version let one
/// unreadable entry abort the whole scan, which presented as "the device never
/// appeared" — so every entry here fails independently.
async fn vhci_bus(super_speed: bool) -> Option<u32> {
    let mut entries = tokio::fs::read_dir(VHCI).await.ok()?;
    let mut buses = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(digits) = name.strip_prefix("usb") else {
            continue;
        };
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(busnum) = read_num(&entry.path().join("busnum")).await else {
            continue;
        };
        let hub_speed = read_num(&entry.path().join("speed")).await.unwrap_or(0);
        buses.push((busnum, hub_speed));
    }
    // The super-speed hub advertises 5000 or more; the other is the one we want
    // for everything else.
    buses
        .into_iter()
        .filter(|(_, hub_speed)| (*hub_speed >= 5000) == super_speed)
        .map(|(busnum, _)| busnum)
        .min()
}

async fn read_num(path: &std::path::Path) -> Result<u32, ()> {
    tokio::fs::read_to_string(path)
        .await
        .map_err(|_| ())?
        .trim()
        .parse()
        .map_err(|_| ())
}

/// The name of the `/dev` node a device offers, as sysfs sees it.
///
/// Returns a bare name (`ttyACM0`, `sda`) rather than a path, because sysfs is
/// the only thing being consulted; whether `/dev` has caught up is a separate
/// question with a separate answer.
async fn node_under(device: &std::path::Path, want: WantedNode) -> Option<std::ffi::OsString> {
    match want {
        WantedNode::Tty { interface } => tty_under(device, interface).await,
        WantedNode::Block => class_node_under(device, "block").await,
        WantedNode::Scsi => class_node_under(device, "scsi_generic").await,
    }
}

/// Find the tty belonging to a USB device, via its interfaces.
///
/// The layout is `<device>/<device>:<cfg>.<n>/tty/ttyACM0` for CDC-ACM and a
/// direct `ttyUSB0` entry for most bridge chips, so both are handled. Every step
/// fails independently: a device has many attribute files that are not
/// directories, and one of them must not end the search.
///
/// `interface` narrows the search to one interface, and matters whenever a
/// device has more than one serial port — on an FT2232H the two channels are
/// interfaces 0 and 1, and on a WROVER-KIT one is JTAG and the other is the
/// console, so taking whichever `readdir` yields first is a coin toss. Nothing
/// falls back to the other interface when the wanted one has no tty: that would
/// restore exactly the ambiguity the number exists to remove.
async fn tty_under(device: &std::path::Path, interface: Option<u8>) -> Option<std::ffi::OsString> {
    let mut interfaces = tokio::fs::read_dir(device).await.ok()?;
    while let Ok(Some(entry)) = interfaces.next_entry().await {
        if let Some(want) = interface {
            if interface_number(&entry.path()).await != Some(want) {
                continue;
            }
        }
        let Ok(mut children) = tokio::fs::read_dir(entry.path()).await else {
            continue;
        };
        while let Ok(Some(child)) = children.next_entry().await {
            let name = child.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == "tty" {
                let Ok(mut inner) = tokio::fs::read_dir(child.path()).await else {
                    continue;
                };
                if let Ok(Some(entry)) = inner.next_entry().await {
                    return Some(entry.file_name());
                }
            } else if name.starts_with("ttyUSB") || name.starts_with("ttyACM") {
                return Some(child.file_name());
            }
        }
    }
    None
}

/// The interface number of a `<device>:<cfg>.<n>` directory, if it is one.
async fn interface_number(dir: &std::path::Path) -> Option<u8> {
    let text = tokio::fs::read_to_string(dir.join("bInterfaceNumber"))
        .await
        .ok()?;
    u8::from_str_radix(text.trim(), 16).ok()
}

/// Find the name under the first `<class>/` directory in a device's subtree —
/// `block` for `sda`, `scsi_generic` for `sg0`.
///
/// Searched rather than addressed by a fixed path because the depth is not
/// fixed: a USB mass-storage node sits at `<intf>/host0/target0:0:0/0:0:0:0/`,
/// and the host, target and lun numbers are all assigned at enumeration.
///
/// The walk never follows symlinks. `read_dir` reports a symlink as such rather
/// than as a directory, and sysfs is full of back-references (`subsystem`,
/// `driver`, `device`) that would otherwise turn this into a cycle.
async fn class_node_under(device: &std::path::Path, class: &str) -> Option<std::ffi::OsString> {
    // Enough for the mass-storage layout above with room to spare; a bound is
    // what keeps a surprising topology from turning into an unbounded walk.
    const MAX_DEPTH: usize = 6;

    let mut queue = vec![(device.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = queue.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if name == class {
                let Ok(mut inner) = tokio::fs::read_dir(entry.path()).await else {
                    continue;
                };
                if let Ok(Some(node)) = inner.next_entry().await {
                    return Some(node.file_name());
                }
            } else if depth < MAX_DEPTH {
                queue.push((entry.path(), depth + 1));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{is_forwarded, node_under, parse_padded};
    use crate::wire::WantedNode;
    use std::path::Path;

    /// Build a directory tree, and write `bInterfaceNumber` for anything that
    /// looks like a USB interface, the way sysfs does.
    fn fake_sysfs(paths: &[&str]) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for path in paths {
            let mut dir = root.path().to_path_buf();
            for part in path.split('/') {
                dir.push(part);
                std::fs::create_dir_all(&dir).unwrap();
                // Interfaces are named `<busid>:<cfg>.<n>`, and sysfs prints
                // the number in hex.
                if let Some(n) = part
                    .split_once(':')
                    .and_then(|(_, config)| config.split_once('.'))
                    .and_then(|(_, n)| n.parse::<u8>().ok())
                {
                    std::fs::write(dir.join("bInterfaceNumber"), format!("{n:02x}\n")).unwrap();
                }
            }
        }
        root
    }

    /// The WROVER-KIT: an FT2232H whose two channels are both serial ports, one
    /// wired to JTAG and one to the console. Picking by directory order gets it
    /// right half the time, which is the bug the interface number exists to fix.
    #[tokio::test]
    async fn the_interface_number_picks_one_tty_of_two() {
        let root = fake_sysfs(&["1-1/1-1:1.0/ttyUSB0", "1-1/1-1:1.1/ttyUSB1"]);
        let device = root.path().join("1-1");

        let console = node_under(&device, WantedNode::Tty { interface: Some(1) }).await;
        assert_eq!(console.as_deref(), Some("ttyUSB1".as_ref()));
        let jtag = node_under(&device, WantedNode::Tty { interface: Some(0) }).await;
        assert_eq!(jtag.as_deref(), Some("ttyUSB0".as_ref()));
    }

    /// Better nothing than the wrong board's console.
    #[tokio::test]
    async fn an_interface_with_no_tty_does_not_fall_back_to_the_other() {
        let root = fake_sysfs(&["1-1/1-1:1.0/ttyUSB0", "1-1/1-1:1.1"]);
        let device = root.path().join("1-1");
        assert_eq!(
            node_under(&device, WantedNode::Tty { interface: Some(1) }).await,
            None
        );
    }

    /// A CDC-ACM tty sits one level deeper than a bridge chip's, and a bench
    /// with a single port names no interface at all.
    #[tokio::test]
    async fn a_lone_tty_is_found_without_being_asked_for_by_interface() {
        let root = fake_sysfs(&["3-2/3-2:1.0/tty/ttyACM0"]);
        let device = root.path().join("3-2");
        assert_eq!(
            node_under(&device, WantedNode::Tty { interface: None })
                .await
                .as_deref(),
            Some("ttyACM0".as_ref())
        );
    }

    /// The USB-SD-Mux: one device, two nodes, and the host, target and lun
    /// numbers in between are assigned at enumeration, so the depth cannot be
    /// hard-coded.
    #[tokio::test]
    async fn a_mass_storage_device_offers_both_its_block_and_its_scsi_node() {
        let root = fake_sysfs(&[
            "3-1.1/3-1.1:1.0/host4/target4:0:0/4:0:0:0/block/sda",
            "3-1.1/3-1.1:1.0/host4/target4:0:0/4:0:0:0/scsi_generic/sg0",
            "3-1.1/power",
        ]);
        let device = root.path().join("3-1.1");

        assert_eq!(
            node_under(&device, WantedNode::Block).await.as_deref(),
            Some("sda".as_ref())
        );
        assert_eq!(
            node_under(&device, WantedNode::Scsi).await.as_deref(),
            Some("sg0".as_ref())
        );
        // And a device that is not storage does not acquire storage nodes.
        let plain = fake_sysfs(&["1-1/1-1:1.0/ttyUSB0"]);
        assert_eq!(
            node_under(&plain.path().join("1-1"), WantedNode::Block).await,
            None
        );
    }

    /// The property that keeps a host from hiding a phantom.
    ///
    /// A bench imported back to the machine it lives on presents a second
    /// device with the same serial, and therefore the same by-id name, as the
    /// board itself. The two are told apart by where they sit in sysfs and by
    /// nothing else.
    #[test]
    fn a_device_forwarded_over_usbip_is_not_local_hardware() {
        assert!(is_forwarded(Path::new(
            "/sys/devices/platform/vhci_hcd.0/usb9/9-1"
        )));
        // A second vhci instance is just as virtual as the first.
        assert!(is_forwarded(Path::new(
            "/sys/devices/platform/vhci_hcd.1/usb11/11-2"
        )));
        // Real hardware hangs off a PCI controller.
        assert!(!is_forwarded(Path::new(
            "/sys/devices/pci0000:00/0000:00:14.0/usb3/3-2/3-2.1"
        )));
        // Nothing else on the platform bus is vhci.
        assert!(!is_forwarded(Path::new(
            "/sys/devices/platform/xhci-hcd.0/usb1/1-1"
        )));
    }

    /// The real table lists both root hubs together.
    const STATUS: &str = "\
hub port sta spd dev      sockfd local_busid
hs  0000 006 002 0007003b 000012 9-1
hs  0001 006 002 0003002a 000013 9-2
hs  0002 004 000 00000000 000000 0-0
ss  0008 004 000 00000000 000000 0-0";

    fn first_free(want: &str) -> Option<u32> {
        STATUS.lines().skip(1).find_map(|line| {
            let mut f = line.split_whitespace();
            let (hub, port, state) = (f.next()?, f.next()?, f.next()?);
            (hub == want && parse_padded(state)? == 4).then_some(parse_padded(port)?)
        })
    }

    #[test]
    fn a_full_speed_device_never_gets_a_superspeed_port() {
        // Both hubs share one table. Attaching a full-speed device to an ss port
        // fails with EBUSY, and the failure only appears once enough hs ports
        // are occupied for a naive scan to run past the end of the hs range —
        // so one device always worked and a second one sometimes did not.
        assert_eq!(first_free("hs"), Some(2));
        assert_eq!(first_free("ss"), Some(8));
    }

    #[test]
    fn zero_padded_sysfs_fields_parse_including_zero() {
        assert_eq!(parse_padded("0000"), Some(0));
        assert_eq!(parse_padded("0008"), Some(8));
        assert_eq!(parse_padded("004"), Some(4));
        assert_eq!(parse_padded("0"), Some(0));
        assert_eq!(parse_padded("x"), None);
    }

    #[test]
    fn a_free_port_is_recognised_in_the_status_table() {
        // Parsing lifted straight from a real /sys/.../status dump; the leading
        // zeroes are what make this fiddly enough to be worth a test.
        let sample = "\
hub port sta spd dev      sockfd local_busid
hs  0000 004 000 00000000 000000 0-0
hs  0001 006 002 00010007 000012 1-2";
        let free: Vec<&str> = sample
            .lines()
            .skip(1)
            .filter(|l| l.split_whitespace().nth(2) == Some("004"))
            .collect();
        assert_eq!(free.len(), 1);
        assert!(free[0].contains("0000"));
    }

    #[test]
    fn usbip_debug_is_not_mistaken_for_a_root_hub() {
        // vhci_hcd.0 contains a file called `usbip_debug`, and a naive
        // `starts_with("usb")` picks it up. That cost an afternoon: the failed
        // read aborted the whole scan and presented as "the device never
        // appeared" rather than as an error.
        let looks_like_a_hub = |name: &str| match name.strip_prefix("usb") {
            Some(rest) => !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()),
            None => false,
        };
        assert!(looks_like_a_hub("usb9"));
        assert!(looks_like_a_hub("usb10"));
        assert!(!looks_like_a_hub("usbip_debug"));
        assert!(!looks_like_a_hub("usb"));
    }
}
