//! The USB/IP wire protocol, and the sysfs handoff.
//!
//! We do not run `usbipd`. It binds the wildcard address with no option to
//! confine it and its protocol has no authentication, so anything that could
//! reach the port could import a bound device — bypassing benchd entirely, which
//! would make a lease a lie. Instead both ends **dial out** to the coordinator,
//! which splices the two sockets, and each side then hands the resulting fd
//! straight to the kernel.
//!
//! That works because neither kernel driver cares how a connection was made:
//!
//! ```text
//! stub_dev.c   usbip_sockfd_store():  sockfd_lookup(sockfd); must be SOCK_STREAM
//! vhci_sysfs.c attach_store():        "port sockfd devid speed"
//!                                     /* @sockfd: an established TCP connection */
//! ```
//!
//! `accept()` on 3240 is merely what `usbipd` happens to do first.
//!
//! Layout is fixed-size big-endian ("network order") throughout, matching
//! `tools/usb/usbip/src/usbip_network.h`. Getting a field width wrong here fails
//! subtly rather than loudly, which is why the structs are unit-tested against
//! their exact byte counts.

use std::io;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// `USBIP_VERSION` — 1.1.1 packed as BCD.
pub const USBIP_VERSION: u16 = 0x0111;

pub const OP_REQ_IMPORT: u16 = 0x8003;
pub const OP_REP_IMPORT: u16 = 0x0003;

pub const ST_OK: u32 = 0x00;
pub const ST_NA: u32 = 0x01;

const SYSFS_BUS_ID_SIZE: usize = 32;
const SYSFS_PATH_MAX: usize = 256;

/// `struct usbip_usb_device` — 312 bytes on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbDevice {
    pub path: String,
    pub busid: String,
    pub busnum: u32,
    pub devnum: u32,
    pub speed: u32,
    pub id_vendor: u16,
    pub id_product: u16,
    pub bcd_device: u16,
    pub b_device_class: u8,
    pub b_device_subclass: u8,
    pub b_device_protocol: u8,
    pub b_configuration_value: u8,
    pub b_num_configurations: u8,
    pub b_num_interfaces: u8,
}

/// Wire size of [`UsbDevice`]. Asserted in tests, because an off-by-one here
/// desynchronises the stream in a way that looks like a hang, not an error.
pub const USB_DEVICE_SIZE: usize = SYSFS_PATH_MAX + SYSFS_BUS_ID_SIZE + 4 + 4 + 4 + 2 + 2 + 2 + 6;

impl UsbDevice {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(USB_DEVICE_SIZE);
        out.extend_from_slice(&fixed(&self.path, SYSFS_PATH_MAX));
        out.extend_from_slice(&fixed(&self.busid, SYSFS_BUS_ID_SIZE));
        out.extend_from_slice(&self.busnum.to_be_bytes());
        out.extend_from_slice(&self.devnum.to_be_bytes());
        out.extend_from_slice(&self.speed.to_be_bytes());
        out.extend_from_slice(&self.id_vendor.to_be_bytes());
        out.extend_from_slice(&self.id_product.to_be_bytes());
        out.extend_from_slice(&self.bcd_device.to_be_bytes());
        out.push(self.b_device_class);
        out.push(self.b_device_subclass);
        out.push(self.b_device_protocol);
        out.push(self.b_configuration_value);
        out.push(self.b_num_configurations);
        out.push(self.b_num_interfaces);
        debug_assert_eq!(out.len(), USB_DEVICE_SIZE);
        out
    }

    pub fn decode(buf: &[u8]) -> io::Result<Self> {
        if buf.len() < USB_DEVICE_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("usbip_usb_device is {} bytes, got {}", USB_DEVICE_SIZE, buf.len()),
            ));
        }
        let mut at = 0;
        let mut take = |n: usize| {
            let slice = &buf[at..at + n];
            at += n;
            slice
        };
        let path = cstr(take(SYSFS_PATH_MAX));
        let busid = cstr(take(SYSFS_BUS_ID_SIZE));
        let busnum = u32::from_be_bytes(take(4).try_into().unwrap());
        let devnum = u32::from_be_bytes(take(4).try_into().unwrap());
        let speed = u32::from_be_bytes(take(4).try_into().unwrap());
        let id_vendor = u16::from_be_bytes(take(2).try_into().unwrap());
        let id_product = u16::from_be_bytes(take(2).try_into().unwrap());
        let bcd_device = u16::from_be_bytes(take(2).try_into().unwrap());
        let rest = take(6);
        Ok(UsbDevice {
            path,
            busid,
            busnum,
            devnum,
            speed,
            id_vendor,
            id_product,
            bcd_device,
            b_device_class: rest[0],
            b_device_subclass: rest[1],
            b_device_protocol: rest[2],
            b_configuration_value: rest[3],
            b_num_configurations: rest[4],
            b_num_interfaces: rest[5],
        })
    }

    /// The identifier vhci wants: bus number in the high half, device in the low.
    pub fn devid(&self) -> u32 {
        (self.busnum << 16) | self.devnum
    }
}

fn fixed(text: &str, width: usize) -> Vec<u8> {
    let mut out = vec![0u8; width];
    let bytes = text.as_bytes();
    let n = bytes.len().min(width - 1);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

fn cstr(buf: &[u8]) -> String {
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// `struct op_common`: version, opcode, status.
async fn send_op_common<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    code: u16,
    status: u32,
) -> io::Result<()> {
    let mut buf = [0u8; 8];
    buf[0..2].copy_from_slice(&USBIP_VERSION.to_be_bytes());
    buf[2..4].copy_from_slice(&code.to_be_bytes());
    buf[4..8].copy_from_slice(&status.to_be_bytes());
    w.write_all(&buf).await
}

async fn recv_op_common<R: AsyncReadExt + Unpin>(r: &mut R) -> io::Result<(u16, u32)> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf).await?;
    let version = u16::from_be_bytes(buf[0..2].try_into().unwrap());
    if version != USBIP_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("usbip version mismatch: peer speaks {version:#06x}, we speak {USBIP_VERSION:#06x}"),
        ));
    }
    Ok((
        u16::from_be_bytes(buf[2..4].try_into().unwrap()),
        u32::from_be_bytes(buf[4..8].try_into().unwrap()),
    ))
}

/// Client half: ask for `busid`, and get the device description back.
pub async fn request_import<S>(stream: &mut S, busid: &str) -> io::Result<UsbDevice>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    send_op_common(stream, OP_REQ_IMPORT, ST_OK).await?;
    stream.write_all(&fixed(busid, SYSFS_BUS_ID_SIZE)).await?;
    stream.flush().await?;

    let (code, status) = recv_op_common(stream).await?;
    if code != OP_REP_IMPORT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected OP_REP_IMPORT, got {code:#06x}"),
        ));
    }
    if status != ST_OK {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("the exporter refused {busid}: status {status}"),
        ));
    }

    let mut buf = vec![0u8; USB_DEVICE_SIZE];
    stream.read_exact(&mut buf).await?;
    UsbDevice::decode(&buf)
}

/// Host half: answer an import request for `device`.
///
/// An **empty busid means "whatever this channel is for"**. Only the host can
/// resolve a by-id path to a busid — the coordinator never touches hardware —
/// so benchd's own client asks by channel rather than by name, and the channel
/// key already identifies exactly one resource of one lease. A non-empty busid
/// is still matched exactly, which keeps stock `usbip attach --remote` working
/// against a benchd host and makes the two implementations differentially
/// testable.
pub async fn accept_import<S>(stream: &mut S, device: &UsbDevice) -> io::Result<String>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let (code, _) = recv_op_common(stream).await?;
    if code != OP_REQ_IMPORT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected OP_REQ_IMPORT, got {code:#06x}"),
        ));
    }
    let mut buf = [0u8; SYSFS_BUS_ID_SIZE];
    stream.read_exact(&mut buf).await?;
    let wanted = cstr(&buf);

    if !wanted.is_empty() && wanted != device.busid {
        send_op_common(stream, OP_REP_IMPORT, ST_NA).await?;
        stream.flush().await?;
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("client asked for {wanted}, this bench exports {}", device.busid),
        ));
    }

    send_op_common(stream, OP_REP_IMPORT, ST_OK).await?;
    stream.write_all(&device.encode()).await?;
    stream.flush().await?;
    Ok(wanted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> UsbDevice {
        UsbDevice {
            path: "/sys/devices/pci0000:00/0000:00:14.0/usb1/1-2".into(),
            busid: "1-2".into(),
            busnum: 1,
            devnum: 7,
            speed: 2,
            id_vendor: 0x303a,
            id_product: 0x1001,
            bcd_device: 0x0101,
            b_device_class: 0xef,
            b_device_subclass: 0x02,
            b_device_protocol: 0x01,
            b_configuration_value: 1,
            b_num_configurations: 1,
            b_num_interfaces: 3,
        }
    }

    #[test]
    fn the_device_struct_is_exactly_the_size_the_kernel_expects() {
        // 256 + 32 + 4 + 4 + 4 + 2 + 2 + 2 + 6. An off-by-one here desynchronises
        // the stream and presents as a hang rather than an error, so pin it.
        assert_eq!(USB_DEVICE_SIZE, 312);
        assert_eq!(sample().encode().len(), 312);
    }

    #[test]
    fn a_device_survives_a_round_trip() {
        let device = sample();
        assert_eq!(UsbDevice::decode(&device.encode()).unwrap(), device);
    }

    #[test]
    fn strings_are_nul_terminated_and_never_overflow_their_field() {
        let long = "x".repeat(500);
        let device = UsbDevice { path: long, ..sample() };
        let encoded = device.encode();
        assert_eq!(encoded.len(), USB_DEVICE_SIZE);
        // Truncated with room for the terminator, so the kernel's strlen stops.
        assert_eq!(encoded[SYSFS_PATH_MAX - 1], 0);
    }

    #[test]
    fn devid_packs_bus_and_device_the_way_vhci_expects() {
        assert_eq!(sample().devid(), (1 << 16) | 7);
    }

    #[tokio::test]
    async fn the_two_halves_of_the_handshake_agree() {
        // The point of testing both sides against each other: if our encoding is
        // wrong in the same way twice this passes, so it is backed up by the
        // byte-layout assertions above and by a live test against real hardware.
        let (mut a, mut b) = tokio::io::duplex(4096);
        let device = sample();

        let server = {
            let device = device.clone();
            tokio::spawn(async move { accept_import(&mut b, &device).await })
        };
        let got = request_import(&mut a, "1-2").await.unwrap();

        assert_eq!(server.await.unwrap().unwrap(), "1-2");
        assert_eq!(got, device);
    }

    #[tokio::test]
    async fn an_empty_busid_means_whatever_this_channel_is_for() {
        // benchd's client asks by channel, because only the host can resolve a
        // by-id path to a busid.
        let (mut a, mut b) = tokio::io::duplex(4096);
        let device = sample();
        let server = {
            let device = device.clone();
            tokio::spawn(async move { accept_import(&mut b, &device).await })
        };
        assert_eq!(request_import(&mut a, "").await.unwrap(), device);
        assert_eq!(server.await.unwrap().unwrap(), "");
    }

    #[tokio::test]
    async fn asking_for_the_wrong_device_is_refused_rather_than_ignored() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let device = sample();
        let server = {
            let device = device.clone();
            tokio::spawn(async move { accept_import(&mut b, &device).await })
        };
        let err = request_import(&mut a, "9-9").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(server.await.unwrap().is_err());
    }
}
