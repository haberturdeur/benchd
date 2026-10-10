//! Making a bench's resources reachable, and taking them back.
//!
//! One case, whichever machine the client is on: the device is already bound to
//! `usbip-host` (see [`crate::hide`]), so we dial the coordinator, answer the
//! client's import request over the relayed connection, and hand the socket to
//! the kernel. We never run `usbipd` and never listen (D5).
//!
//! There is deliberately no co-located shortcut. A bench device stays hidden —
//! stub-bound, with no tty — for the whole life of this process, so there is no
//! local inode for a same-machine client to bind-mount even in principle.
//!
//! Nothing here changes which driver owns a device. [`crate::hide`] binds the
//! stub at startup and holds that binding until the process ends, so a lease
//! only ever attaches a socket to a device that is already bound, and ending
//! one only ever takes that socket away. Two owners of one binding is what made
//! the first lease to end give its board back to `cdc_acm`, tty and all, with
//! nothing left that would hide it again.
//!
//! Everything is **epoch-fenced and idempotent**. A delayed instruction for a
//! superseded lease must be dropped, not obeyed: obeying it would hand live
//! hardware to an agent whose lease is gone (D7).

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use benchd_core::lease::{Epoch, LeaseId, SessionId};
use benchd_core::model::Resource;
use benchd_core::sysfs;
use benchd_core::usbip::{self, UsbDevice};
use benchd_core::wire::{BenchSpec, ChannelHello, ChannelKey, ChannelSide, Outcome};
use futures::SinkExt;
use tokio_util::codec::{FramedWrite, LinesCodec};

pub struct Exports {
    spec: BenchSpec,
    coordinator: String,
    /// Resource name -> busid, resolved *before* the bench was hidden.
    ///
    /// It cannot be resolved later. A serial resource is named by a tty, and
    /// hiding takes the device off its driver so the tty and the `/dev/serial`
    /// symlink pointing at it both disappear — leaving nothing to walk up from.
    /// Recomputing here silently dropped every serial resource, and the bench
    /// then exported whatever else it had while the client sat waiting on a
    /// channel nobody was ever going to dial.
    busids: BTreeMap<String, String>,
    /// Highest epoch seen for this bench. Anything lower is stale.
    seen: Epoch,
    active: BTreeMap<LeaseId, Active>,
}

struct Active {
    /// The epoch this was exported at. Kept for diagnostics: when fencing goes
    /// wrong, the first question is always which epoch a device was handed out
    /// under versus which one the coordinator thinks is current.
    epoch: Epoch,
    session: SessionId,
    /// Busids handed to the kernel, needing an explicit teardown.
    exported: Vec<String>,
}

impl Exports {
    pub fn new(spec: BenchSpec, coordinator: String, busids: BTreeMap<String, String>) -> Self {
        Exports {
            spec,
            coordinator,
            busids,
            seen: Epoch(0),
            active: BTreeMap::new(),
        }
    }

    /// Drop a socket a previous incarnation of this process left attached.
    ///
    /// The kernel keeps its own reference to a handed-over socket, so an export
    /// outlives the process that made it. Without this, a restart would leave
    /// hardware reachable by an agent whose lease no longer exists (D6).
    ///
    /// It looks for a socket rather than for a binding, and that is the whole
    /// distinction: by the time we get here every one of this bench's devices is
    /// bound to the stub, because hiding just bound them. Testing for the
    /// binding instead released every device microseconds after hiding it. What
    /// can genuinely survive is a socket on a device that `hide` *adopted* — it
    /// leaves an already-bound device alone, so a host killed mid-lease whose
    /// record could not be replayed comes back with the export still live.
    pub async fn clear_stale(&mut self) {
        for (_, busid) in self.busids() {
            if sysfs::stub_has_socket(&busid).await {
                tracing::warn!(%busid, "clearing a stale export from a previous run");
                sysfs::stub_detach(&busid).await;
            }
        }
    }

    /// Every USB busid this bench can export, by resource name.
    fn busids(&self) -> Vec<(String, String)> {
        self.busids
            .iter()
            .map(|(name, busid)| (name.clone(), busid.clone()))
            .collect()
    }

    fn fence(&mut self, epoch: Epoch) -> Result<(), Outcome> {
        if epoch < self.seen {
            tracing::warn!(?epoch, seen = ?self.seen, "dropping a stale instruction");
            return Err(Outcome::Stale { seen: self.seen });
        }
        self.seen = epoch;
        Ok(())
    }

    pub async fn export(
        &mut self,
        lease: LeaseId,
        epoch: Epoch,
        session: SessionId,
        channels: BTreeMap<String, ChannelKey>,
    ) -> Outcome {
        if let Err(stale) = self.fence(epoch) {
            return stale;
        }
        if self.active.contains_key(&lease) {
            return Outcome::Ok; // idempotent: a retry is not an error
        }

        let busids = self.busids();
        if busids.is_empty() {
            // Registration resolves every resource to a busid and refuses the
            // bench when it cannot, so this is a guard rather than a path.
            return Outcome::Failed {
                detail: format!("bench {} has no USB device to export", self.spec.id),
            };
        }

        // One device, one export, however many resources name it. A USB-SD-Mux
        // is two resources — the SCSI node that switches the card and the block
        // node that holds it — but USB/IP forwards whole devices, so binding it
        // twice would fail on the second attempt and minting two channels would
        // leave one of them with nobody to pair with.
        let mut exported = Vec::new();
        let mut seen: std::collections::BTreeSet<&str> = Default::default();
        for (resource, busid) in &busids {
            if !seen.insert(busid.as_str()) {
                continue;
            }
            // The coordinator minted this key and gave the same one to the
            // client; it is random rather than derived, so nobody else can
            // guess it and race us to the rendezvous.
            let Some(key) = channels.get(resource).cloned() else {
                self.tear_down(&exported).await;
                return Outcome::Failed {
                    detail: format!("no channel key was issued for resource {resource:?}"),
                };
            };

            // The device is bound already — hiding did it at startup and holds
            // that binding for the life of this process — so exporting has
            // nothing to bind. If it is somehow not bound then this bench is
            // not hidden, and binding it here would take back an ownership
            // hiding never gave up: the next lease to end would hand the board
            // to `cdc_acm` and put a tty on this machine for good.
            if !sysfs::is_bound(busid) {
                self.tear_down(&exported).await;
                return Outcome::Failed {
                    detail: format!(
                        "{busid} is not bound to the usbip stub, so resource {resource:?} is \
                         not hidden and must not be exported"
                    ),
                };
            }

            // Detaching a device that has no socket attached is a no-op, so a
            // busid can join the teardown list before there is anything on it
            // to take away.
            exported.push(busid.clone());

            let device = match describe(busid).await {
                Ok(device) => device,
                Err(err) => {
                    self.tear_down(&exported).await;
                    return Outcome::Failed {
                        detail: format!("reading {busid}: {err}"),
                    };
                }
            };
            if let Err(err) = self.serve_channel(&key, device).await {
                self.tear_down(&exported).await;
                return Outcome::Failed {
                    detail: format!("exporting {busid}: {err}"),
                };
            }
        }

        tracing::info!(
            bench = %self.spec.id, %lease, ?epoch,
            devices = exported.len(), "exported (relayed)"
        );
        self.active.insert(
            lease,
            Active {
                epoch,
                session,
                exported,
            },
        );
        Outcome::Ok
    }

    /// Dial the coordinator, answer the import request, hand over the socket.
    async fn serve_channel(
        &self,
        key: &ChannelKey,
        device: UsbDevice,
    ) -> Result<(), std::io::Error> {
        let mut stream = tokio::net::TcpStream::connect(&self.coordinator).await?;
        stream.set_nodelay(true).ok();
        benchd_core::protocol::connect(&mut stream).await?;
        let (read, write) = stream.into_split();

        // One line of JSON to identify the channel, then the socket is opaque
        // USB/IP bytes for the rest of its life.
        let mut sink = FramedWrite::new(write, LinesCodec::new());
        let hello = ChannelHello {
            channel: key.clone(),
            side: ChannelSide::Host,
        };
        sink.send(serde_json::to_string(&hello)?)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let mut stream = read
            .reunite(sink.into_inner())
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let busid = device.busid.clone();
        tracing::info!(channel = %key.0, %busid, "data channel open; awaiting import request");
        usbip::accept_import(&mut stream, &device).await?;
        sysfs::stub_attach(&busid, stream).await?;
        tracing::info!(channel = %key.0, %busid, "handed the socket to the kernel");
        Ok(())
    }

    /// Take back every socket handed out for one lease.
    ///
    /// Mandatory rather than tidy, and idempotent for the same reason: the
    /// kernel holds its own reference to a handed-over socket, and the lease
    /// reaper races voluntary releases, so this may run twice and may not fail.
    /// It leaves the devices bound — that binding belongs to hiding, and giving
    /// it back here would put a tty on this machine for a board nothing is
    /// going to hide again.
    async fn tear_down(&self, busids: &[String]) {
        for busid in busids {
            sysfs::stub_detach(busid).await;
        }
    }

    pub async fn unexport(&mut self, lease: LeaseId, epoch: Epoch) -> Outcome {
        if let Err(stale) = self.fence(epoch) {
            return stale;
        }
        // Unexporting something we do not have is a no-op, not an error: the
        // reaper races voluntary releases and neither path may fail.
        let Some(active) = self.active.remove(&lease) else {
            return Outcome::Ok;
        };
        self.tear_down(&active.exported).await;
        tracing::info!(
            bench = %self.spec.id, %lease,
            exported_at = ?active.epoch, session = %active.session,
            "unexported"
        );
        Outcome::Ok
    }

    /// The coordinator is gone, so every lease it granted is void (D6).
    ///
    /// **Also resets the epoch high-water mark.** Epochs are the coordinator's
    /// counters and it is stateless across restarts, so a fresh coordinator
    /// starts again at 1. A host that kept its old watermark would reject every
    /// instruction from the new coordinator as stale — permanently, since the
    /// counter never catches up. Fencing only has to order instructions from
    /// *one* coordinator incarnation, and losing the connection ends that
    /// incarnation as far as we can tell.
    pub async fn release_all(&mut self) {
        let leases: Vec<LeaseId> = self.active.keys().copied().collect();
        for lease in leases {
            let epoch = self.seen;
            self.unexport(lease, epoch).await;
        }
        self.seen = Epoch(0);
    }
}

/// Recover a device this bench owns that was left bound to the USB/IP stub.
///
/// A host killed mid-export (SIGKILL, power cut, an OOM) never runs its
/// teardown, so the device stays bound to `usbip-host` and has no tty. That is
/// unrecoverable by the normal path, because the normal path *identifies* the
/// device through the tty that no longer exists — so the host refuses to start,
/// systemd restarts it, it refuses again, and the board is dead until someone
/// finds it by hand.
///
/// So before resolving anything, look for a stub-bound device whose USB serial
/// appears in one of this bench's by-id paths, and release it. by-id names
/// embed the serial, which is what makes the match possible without a tty.
pub async fn recover_orphans(spec: &BenchSpec) {
    let wanted: Vec<String> = spec
        .resources
        .values()
        .filter_map(|r| match r {
            Resource::Serial { path, .. } => Some(path.to_string_lossy().into_owned()),
            Resource::Usb { .. } => None,
        })
        .collect();
    if wanted.is_empty() {
        return;
    }

    // Match by USB serial, which is what by-id names embed. Two states need
    // recovering and neither can be found through a tty, because neither has
    // one: a device still bound to the stub, and a device that was unbound but
    // never got its driver back.
    for (busid, serial) in sysfs::devices_by_serial().await {
        if !wanted.iter().any(|path| path.contains(&serial)) {
            continue;
        }
        // A copy of this bench forwarded back to it carries the same serial as
        // the board itself, so matching on serial alone finds both. Recovering
        // the copy would at best waste the re-probe budget on a device that is
        // about to disappear, and at worst report the wrong thing.
        if sysfs::is_forwarded_busid(&busid) {
            continue;
        }
        if sysfs::is_bound(&busid) {
            tracing::warn!(
                %busid, %serial,
                "this bench's device was left bound to the usbip stub; releasing it"
            );
            if let Err(detail) = sysfs::unbind(&busid).await {
                tracing::error!(%busid, %detail, "could not release this device");
                continue;
            }
        } else if !sysfs::has_tty(&busid).await {
            tracing::warn!(
                %busid, %serial,
                "this bench's device has no driver; re-probing it"
            );
            if !sysfs::reattach(&busid).await {
                continue;
            }
        } else {
            continue;
        }
        // udev has to recreate the device node before anything resolves it.
        // There is nothing to wait for when the device did not come back, and
        // both calls above have already said so.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    }
}

/// Every USB busid this bench can export, by resource name.
///
/// Must be resolved before anything is exported: the mapping goes through the
/// tty, and exporting removes it.
///
/// Every resource must resolve, and one that does not fails the whole bench.
/// This map is exactly what hiding hides, so a resource quietly missing from it
/// is a board left with a live tty on a bench that has just reported itself
/// hidden — and a map that comes out empty gives a bench which registers,
/// matches every claim, fails all of them, and goes on absorbing them.
pub fn busids_for(spec: &BenchSpec) -> Result<BTreeMap<String, String>> {
    if spec.resources.is_empty() {
        anyhow::bail!(
            "bench {} declares no resources, so there is nothing to hide or to lease",
            spec.id
        );
    }
    let mut busids = BTreeMap::new();
    for (name, resource) in &spec.resources {
        let busid = match resource {
            Resource::Usb { busid, .. } => busid.clone(),
            Resource::Serial { path, .. } => busid_for_tty(path)
                .with_context(|| format!("resource {name:?} has no USB device to hide"))?,
        };
        busids.insert(name.clone(), busid);
    }
    Ok(busids)
}

/// Walk from a `/dev/serial` symlink up to the USB device that owns it.
///
/// `/sys/class/tty/ttyACM0/device` is the *interface* (`1-2:1.0`); the device is
/// its parent, and the busid is the part before the colon.
fn busid_for_tty(path: &std::path::Path) -> Result<String> {
    let tty = std::fs::canonicalize(path)
        .with_context(|| format!("{} is not present", path.display()))?;
    let dir = sysfs::usb_device_of_tty(&tty).with_context(|| {
        format!(
            "{} is {}, which no USB device owns",
            path.display(),
            tty.display()
        )
    })?;
    // Registration refuses a forwarded device already, so reaching here means
    // one appeared under a name this bench had resolved. Nothing good can be
    // done with it: binding the stub to a virtual device would leave the real
    // board untouched and visible, which is why this is fatal rather than a
    // resource silently dropped from the map.
    if sysfs::is_forwarded(&dir) {
        anyhow::bail!(
            "{} resolves to a device forwarded in over USB/IP, not to local hardware",
            path.display()
        );
    }
    dir.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .with_context(|| format!("{} has no readable bus id", dir.display()))
}

/// Read a bound device's descriptors out of sysfs.
async fn describe(busid: &str) -> std::io::Result<UsbDevice> {
    let base = format!("/sys/bus/usb/devices/{busid}");
    async fn field(base: &str, name: &str) -> std::io::Result<String> {
        Ok(tokio::fs::read_to_string(format!("{base}/{name}"))
            .await?
            .trim()
            .to_string())
    }
    fn hex(text: &str) -> u16 {
        u16::from_str_radix(text.trim(), 16).unwrap_or(0)
    }
    fn dec<T: std::str::FromStr + Default>(text: &str) -> T {
        text.trim().parse().unwrap_or_default()
    }

    let speed_text = field(&base, "speed").await.unwrap_or_default();
    Ok(UsbDevice {
        path: base.clone(),
        busid: busid.to_string(),
        busnum: dec(&field(&base, "busnum").await?),
        devnum: dec(&field(&base, "devnum").await?),
        // usb_device_speed: 1 = low, 2 = full, 3 = high, 5 = super, 6 = super
        // plus. The client reserves a vhci port from this number, and the
        // kernel puts anything from 5 up on the super-speed hub, so the two
        // faster speeds must stay distinct from the rest.
        speed: match speed_text.as_str() {
            "1.5" => 1,
            "12" => 2,
            "480" => 3,
            "5000" => 5,
            "10000" => 6,
            _ => 3,
        },
        id_vendor: hex(&field(&base, "idVendor").await?),
        id_product: hex(&field(&base, "idProduct").await?),
        bcd_device: hex(&field(&base, "bcdDevice").await.unwrap_or_default()),
        b_device_class: dec(&field(&base, "bDeviceClass").await.unwrap_or_default()),
        b_device_subclass: dec(&field(&base, "bDeviceSubClass").await.unwrap_or_default()),
        b_device_protocol: dec(&field(&base, "bDeviceProtocol").await.unwrap_or_default()),
        b_configuration_value: dec(&field(&base, "bConfigurationValue")
            .await
            .unwrap_or_default()),
        b_num_configurations: dec(&field(&base, "bNumConfigurations").await.unwrap_or_default()),
        b_num_interfaces: dec(&field(&base, "bNumInterfaces").await.unwrap_or_default()),
    })
}

#[cfg(test)]
mod tests {
    use super::busids_for;
    use benchd_core::model::{Resource, UsbNode};
    use benchd_core::wire::BenchSpec;
    use std::collections::BTreeMap;

    fn spec(resources: BTreeMap<String, Resource>) -> BenchSpec {
        BenchSpec {
            id: "esp32s3-sdmux".into(),
            description: String::new(),
            docs: String::new(),
            tags: Vec::new(),
            resources,
        }
    }

    fn sdmux() -> (String, Resource) {
        (
            "sdmux".into(),
            Resource::Usb {
                busid: "3-1.1".into(),
                node: UsbNode::Scsi,
            },
        )
    }

    /// The map is what hiding hides, so a resource that falls out of it is a
    /// board left with a live tty on a bench reporting itself hidden. Dropping
    /// it silently is what made that possible; the error has to name it, or
    /// nobody can tell which of three boards is still exposed.
    #[test]
    fn a_resource_that_resolves_to_no_device_fails_the_whole_bench() {
        let mut resources = BTreeMap::new();
        let (name, resource) = sdmux();
        resources.insert(name, resource);
        resources.insert(
            "console".into(),
            Resource::Serial {
                path: "/nonexistent/by-path/platform-xhci-hcd.0-usb-0:2:1.0".into(),
                serial: None,
                interface: None,
            },
        );

        let err = busids_for(&spec(resources)).expect_err("an unresolvable resource is fatal");
        let err = format!("{err:#}");
        assert!(err.contains("console"), "{err}");
    }

    /// The degenerate bench: nothing to hide, so nothing to lease either. It
    /// used to register, log itself hidden with zero devices, and then swallow
    /// every claim that matched its tags.
    #[test]
    fn a_bench_with_no_resources_is_refused() {
        let err = busids_for(&spec(BTreeMap::new())).expect_err("nothing to hide is fatal");
        assert!(format!("{err:#}").contains("no resources"), "{err}");
    }

    /// Two resources on one device keep both names and one busid: the SCSI node
    /// switches a USB-SD-Mux and the block node holds its card.
    #[test]
    fn resources_named_by_busid_need_no_tty_to_resolve() {
        let mut resources = BTreeMap::new();
        let (name, resource) = sdmux();
        resources.insert(name, resource);
        resources.insert(
            "sdcard".into(),
            Resource::Usb {
                busid: "3-1.1".into(),
                node: UsbNode::Block,
            },
        );

        let busids = busids_for(&spec(resources)).expect("busids resolve on their own");
        assert_eq!(busids.get("sdmux").map(String::as_str), Some("3-1.1"));
        assert_eq!(busids.get("sdcard").map(String::as_str), Some("3-1.1"));
    }
}
