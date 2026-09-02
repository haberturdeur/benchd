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
            Err(io::Error::new(io::ErrorKind::TimedOut, "sysfs write timed out"))
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
    let _ = write_sysfs(format!("{STUB}/match_busid"), &format!("del {busid}")).await;
    reattach(busid).await;
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
    let Ok(mut entries) = tokio::fs::read_dir(format!("/sys/bus/usb/devices/{busid}")).await
    else {
        return false;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Interfaces are named like `3-2:1.0`.
        if !name.starts_with(busid) || !name.contains(':') {
            continue;
        }
        if tokio::fs::metadata(entry.path().join("driver")).await.is_ok() {
            return true;
        }
    }
    false
}

/// Whether this device currently presents a serial port.
pub async fn has_tty(busid: &str) -> bool {
    tty_under(&PathBuf::from(format!("/sys/bus/usb/devices/{busid}"))).await.is_some()
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
        let (Some(hub), Some(port), Some(state)) =
            (fields.next(), fields.next(), fields.next())
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
        format!(
            "no free {want} vhci port; every virtual slot for this device speed is in use"
        ),
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
pub async fn vhci_attach(
    port: u32,
    stream: TcpStream,
    devid: u32,
    speed: u32,
) -> io::Result<()> {
    let std_stream = stream.into_std()?;
    std_stream.set_nonblocking(false)?;
    let fd = std_stream.as_raw_fd();

    let result =
        write_sysfs(format!("{VHCI}/attach"), &format!("{port} {fd} {devid} {speed}")).await;

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
    use super::parse_padded;

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
