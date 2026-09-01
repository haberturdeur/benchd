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
use std::path::PathBuf;

use tokio::net::TcpStream;

const STUB: &str = "/sys/bus/usb/drivers/usbip-host";
const VHCI: &str = "/sys/devices/platform/vhci_hcd.0";

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
    let _ = tokio::fs::write(format!("/sys/bus/usb/devices/{busid}/driver/unbind"), busid).await;
    tokio::fs::write(format!("{STUB}/match_busid"), format!("add {busid}")).await.ok();
    tokio::fs::write(format!("{STUB}/bind"), busid).await
}

pub async fn unbind(busid: &str) {
    // Stop the kernel pumping first, then release the device. The reverse order
    // leaves a live socket attached to a device we no longer own.
    let _ = tokio::fs::write(stub_path(busid).join("usbip_sockfd"), "-1").await;
    let _ = tokio::fs::write(format!("{STUB}/unbind"), busid).await;
    let _ = tokio::fs::write(format!("{STUB}/match_busid"), format!("del {busid}")).await;
    // Put the device back under its normal driver so the board is usable
    // locally again once the lease is over.
    let _ = tokio::fs::write("/sys/bus/usb/drivers_probe", busid).await;
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

    let result =
        tokio::fs::write(stub_path(busid).join("usbip_sockfd"), fd.to_string()).await;

    // The kernel took its own reference on success, so our copy could be closed;
    // we leak it anyway rather than risk a race between close and the kernel's
    // lookup. One fd per active lease is not a leak worth chasing.
    let _ = std_stream.into_raw_fd();
    result
}

// -- client side ------------------------------------------------------------

/// A free virtual port on the local vhci hub.
///
/// `status` looks like:
///
/// ```text
/// hub port sta spd dev      sockfd local_busid
/// hs  0000 004 000 00000000 000000 0-0
/// ```
///
/// where `sta` 4 is `VDEV_ST_NULL` — unused.
pub async fn free_vhci_port() -> io::Result<u32> {
    let status = tokio::fs::read_to_string(format!("{VHCI}/status")).await?;
    for line in status.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let _hub = fields.next();
        let Some(port) = fields.next() else { continue };
        let Some(state) = fields.next() else { continue };
        if state.trim_start_matches('0') == "4" || state == "004" {
            if let Ok(port) = port.trim_start_matches('0').parse::<u32>() {
                return Ok(port);
            }
            // Port "0000" trims to empty.
            if port.chars().all(|c| c == '0') {
                return Ok(0);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "no free vhci port; every virtual slot is in use",
    ))
}

/// Attach a connected socket to the local vhci hub, importing the device.
///
/// Consumes the stream for the same reason as [`stub_attach`].
pub async fn vhci_attach(
    port: u32,
    stream: TcpStream,
    devid: u32,
    speed: u32,
) -> io::Result<()> {
    let std_stream = stream.into_std()?;
    std_stream.set_nonblocking(false)?;
    let fd = std_stream.as_raw_fd();

    let result = tokio::fs::write(
        format!("{VHCI}/attach"),
        format!("{port} {fd} {devid} {speed}"),
    )
    .await;

    let _ = std_stream.into_raw_fd();
    result
}

pub async fn vhci_detach(port: u32) {
    let _ = tokio::fs::write(format!("{VHCI}/detach"), port.to_string()).await;
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
            (state.trim_start_matches('0') == "6")
                .then(|| port.trim_start_matches('0').parse().unwrap_or(0))
        })
        .collect()
}

/// Wait for the serial device a freshly imported device produces, on `port`.
///
/// The kernel enumerates asynchronously, so nothing exists the instant `attach`
/// returns.
///
/// **Found by sysfs position, not by diffing `/dev/serial/by-id`.** A by-id name
/// is built from vendor, product and serial number, so a device imported from
/// another machine can produce *exactly* the name of one that just disappeared
/// locally — which is precisely what happens when a bench is forwarded back to
/// the machine it lives on. Diffing sees nothing; the port tells the truth.
///
/// vhci exposes two root hubs, high speed and super speed. A device on rhport
/// `p` of a hub whose bus is `b` enumerates as `b-(p+1)`.
pub async fn wait_for_vhci_tty(
    port: u32,
    speed: u32,
    timeout: std::time::Duration,
) -> Option<PathBuf> {
    let super_speed = speed >= 5;
    let deadline = tokio::time::Instant::now() + timeout;

    while tokio::time::Instant::now() < deadline {
        if let Some(bus) = vhci_bus(super_speed).await {
            let device = PathBuf::from(VHCI)
                .join(format!("usb{bus}"))
                .join(format!("{bus}-{}", port + 1));
            if let Some(tty) = tty_under(&device).await {
                // Let udev finish applying permissions before anyone opens it.
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                return Some(tty);
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
        let Some(digits) = name.strip_prefix("usb") else { continue };
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(busnum) = read_num(&entry.path().join("busnum")).await else { continue };
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

/// Find the `/dev/tty*` belonging to a USB device, via its interfaces.
///
/// The layout is `<device>/<device>:<cfg>.<n>/tty/ttyACM0` for CDC-ACM and a
/// direct `ttyUSB0` entry for most bridge chips, so both are handled. Every step
/// fails independently: a device has many attribute files that are not
/// directories, and one of them must not end the search.
async fn tty_under(device: &std::path::Path) -> Option<PathBuf> {
    let mut interfaces = tokio::fs::read_dir(device).await.ok()?;
    while let Ok(Some(interface)) = interfaces.next_entry().await {
        let Ok(mut children) = tokio::fs::read_dir(interface.path()).await else {
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
                    let dev = PathBuf::from("/dev").join(entry.file_name());
                    if tokio::fs::metadata(&dev).await.is_ok() {
                        return Some(dev);
                    }
                }
            } else if name.starts_with("ttyUSB") || name.starts_with("ttyACM") {
                let dev = PathBuf::from("/dev").join(name);
                if tokio::fs::metadata(&dev).await.is_ok() {
                    return Some(dev);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
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
