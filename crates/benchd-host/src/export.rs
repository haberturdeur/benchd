//! Making a bench's resources reachable, and taking them back.
//!
//! Two cases, and the difference is where the client is:
//!
//! * **Co-located** — nothing to do. The client bind-mounts the real inode, so
//!   `export` is bookkeeping only.
//! * **Remote** — the device is bound to `usbip-host`, we dial the coordinator,
//!   answer the client's import request over the relayed connection, and hand
//!   the socket to the kernel. We never run `usbipd` and never listen (D5).
//!
//! Everything is **epoch-fenced and idempotent**. A delayed instruction for a
//! superseded lease must be dropped, not obeyed: obeying it would hand live
//! hardware to an agent whose lease is gone (D7).

use std::collections::BTreeMap;

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
    pub fn new(spec: BenchSpec, coordinator: String) -> Self {
        Exports { spec, coordinator, seen: Epoch(0), active: BTreeMap::new() }
    }

    /// Undo anything a previous incarnation of this process left behind.
    ///
    /// The kernel keeps its own reference to a handed-over socket, so an export
    /// outlives the process that made it. Without this, a restart would leave
    /// hardware reachable by an agent whose lease no longer exists (D6).
    pub async fn clear_stale(&mut self) {
        for (_, busid) in self.busids() {
            if sysfs::is_bound(&busid) {
                tracing::warn!(%busid, "clearing a stale export from a previous run");
                sysfs::unbind(&busid).await;
            }
        }
    }

    /// Every USB busid this bench can export.
    ///
    /// A serial resource identified by a by-id path is resolved to its USB
    /// device here, so a bench does not have to be written twice to work both
    /// locally and remotely.
    fn busids(&self) -> Vec<(String, String)> {
        self.spec
            .resources
            .iter()
            .filter_map(|(name, r)| {
                let busid = match r {
                    Resource::Usb { busid } => Some(busid.clone()),
                    Resource::Serial { path, .. } => busid_for_tty(path),
                }?;
                Some((name.clone(), busid))
            })
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

        if channels.is_empty() {
            tracing::info!(bench = %self.spec.id, %lease, ?epoch, "exported (co-located)");
            self.active.insert(lease, Active { epoch, session, exported: Vec::new() });
            return Outcome::Ok;
        }

        let busids = self.busids();
        if busids.is_empty() {
            return Outcome::Failed {
                detail: format!(
                    "bench {} has no USB device to export remotely \
                     (a serial resource must resolve to a USB device)",
                    self.spec.id
                ),
            };
        }

        let mut exported = Vec::new();
        for (resource, busid) in &busids {
            // The coordinator minted this key and gave the same one to the
            // client; it is random rather than derived, so nobody else can
            // guess it and race us to the rendezvous.
            let Some(key) = channels.get(resource).cloned() else {
                self.tear_down(&exported).await;
                return Outcome::Failed {
                    detail: format!("no channel key was issued for resource {resource:?}"),
                };
            };

            if let Err(err) = sysfs::bind(busid).await {
                self.tear_down(&exported).await;
                // `bind` can fail after detaching the device from its normal
                // driver, so the busid it was working on has to be cleaned up
                // too — otherwise the board vanishes from the machine until the
                // host process restarts.
                sysfs::unbind(busid).await;
                return Outcome::Failed { detail: format!("usbip bind {busid}: {err}") };
            }

            // From here the device IS bound, so every failure path below must
            // include this busid in the teardown, not just the ones that
            // already succeeded.
            let mut bound = exported.clone();
            bound.push(busid.clone());

            let device = match describe(busid).await {
                Ok(device) => device,
                Err(err) => {
                    self.tear_down(&bound).await;
                    return Outcome::Failed { detail: format!("reading {busid}: {err}") };
                }
            };
            match self.serve_channel(&key, device).await {
                Ok(()) => exported.push(busid.clone()),
                Err(err) => {
                    self.tear_down(&bound).await;
                    return Outcome::Failed { detail: format!("exporting {busid}: {err}") };
                }
            }
        }

        tracing::info!(
            bench = %self.spec.id, %lease, ?epoch,
            devices = exported.len(), "exported (relayed)"
        );
        self.active.insert(lease, Active { epoch, session, exported });
        Outcome::Ok
    }

    /// Dial the coordinator, answer the import request, hand over the socket.
    async fn serve_channel(
        &self,
        key: &ChannelKey,
        device: UsbDevice,
    ) -> Result<(), std::io::Error> {
        let stream = tokio::net::TcpStream::connect(&self.coordinator).await?;
        stream.set_nodelay(true).ok();
        let (read, write) = stream.into_split();

        // One line of JSON to identify the channel, then the socket is opaque
        // USB/IP bytes for the rest of its life.
        let mut sink = FramedWrite::new(write, LinesCodec::new());
        let hello = ChannelHello { channel: key.clone(), side: ChannelSide::Host };
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

    async fn tear_down(&self, busids: &[String]) {
        for busid in busids {
            sysfs::unbind(busid).await;
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
        if sysfs::is_bound(&busid) {
            tracing::warn!(
                %busid, %serial,
                "this bench's device was left bound to the usbip stub; releasing it"
            );
            sysfs::unbind(&busid).await;
        } else if !sysfs::has_tty(&busid).await {
            tracing::warn!(
                %busid, %serial,
                "this bench's device has no driver; re-probing it"
            );
            sysfs::reattach(&busid).await;
        } else {
            continue;
        }
        // udev has to recreate the device node before anything resolves it.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    }
}

/// Walk from a `/dev/serial/by-id` symlink up to the USB device that owns it.
///
/// `/sys/class/tty/ttyACM0/device` is the *interface* (`1-2:1.0`); the device is
/// its parent, and the busid is the part before the colon.
fn busid_for_tty(by_id: &std::path::Path) -> Option<String> {
    let tty = std::fs::canonicalize(by_id).ok()?;
    let name = tty.file_name()?.to_str()?;
    let link = std::fs::read_link(format!("/sys/class/tty/{name}/device")).ok()?;
    let interface = link.file_name()?.to_str()?;
    interface.split(':').next().map(str::to_string)
}

/// Read a bound device's descriptors out of sysfs.
async fn describe(busid: &str) -> std::io::Result<UsbDevice> {
    let base = format!("/sys/bus/usb/devices/{busid}");
    async fn field(base: &str, name: &str) -> std::io::Result<String> {
        Ok(tokio::fs::read_to_string(format!("{base}/{name}")).await?.trim().to_string())
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
        // usb_device_speed: 1 = low, 2 = full, 3 = high, 5 = super.
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
        b_configuration_value: dec(
            &field(&base, "bConfigurationValue").await.unwrap_or_default(),
        ),
        b_num_configurations: dec(
            &field(&base, "bNumConfigurations").await.unwrap_or_default(),
        ),
        b_num_interfaces: dec(&field(&base, "bNumInterfaces").await.unwrap_or_default()),
    })
}
