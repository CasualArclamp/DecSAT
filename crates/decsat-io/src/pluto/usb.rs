// made by CasualArclamp
//! The Pluto's IIOD over USB, without libiio or libusb: the firmware's iiod
//! runs on a FunctionFS interface named "IIO" (`iiod -F /dev/iio_ffs`, three
//! bulk endpoint pairs), and every pair carries the same text protocol as
//! TCP ([`super::iiod`]). From libiio v0.25 `usb.c` (LGPL-2.1, Analog
//! Devices):
//!
//! * the interface is the one whose iInterface string is "IIO";
//! * its endpoints pair up in order, IN then OUT: pair *i* is pipe *i*;
//! * vendor requests to the interface: 0 resets every pipe, 1 opens pipe
//!   `wValue` (2 closes one);
//! * pipe 0 is the context's (attributes); a buffer gets a pipe of its own.
//!
//! On Windows the interface needs the WinUSB driver: ADI's Pluto driver
//! package binds it (`pluto-usbd.inf`, `VID_0456&PID_B673&MI_05`); Zadig on
//! that interface works too.

use std::io;
use std::sync::Arc;

use futures_lite::future::block_on;
use nusb::transfer::{Completion, ControlOut, ControlType, Queue, Recipient, RequestBuffer};

use super::iiod::Link;
use decsat_core::{Error, Result};

/// ADALM-PLUTO (Pluto+ firmware keeps the same IDs).
pub const VID: u16 = 0x0456;
pub const PID: u16 = 0xb673;

const REQ_RESET_PIPES: u8 = 0;
const REQ_OPEN_PIPE: u8 = 1;

fn err(e: impl std::fmt::Display) -> Error {
    Error::Other(format!("Pluto USB: {e}"))
}

/// Whether a Pluto is enumerated on USB at all (for `auto`).
pub fn present() -> bool {
    nusb::list_devices()
        .map(|mut d| d.any(|d| d.vendor_id() == VID && d.product_id() == PID))
        .unwrap_or(false)
}

/// The claimed IIO interface, shared by its pipes.
pub struct UsbIio {
    interface: Arc<nusb::Interface>,
    number: u8,
    /// (IN, OUT) endpoint addresses, pipe by pipe.
    pairs: Vec<(u8, u8)>,
}

impl UsbIio {
    /// Find the first Pluto, claim its IIO interface and reset its pipes.
    pub fn open() -> Result<UsbIio> {
        let info = nusb::list_devices()
            .map_err(err)?
            .find(|d| d.vendor_id() == VID && d.product_id() == PID)
            .ok_or_else(|| err("no ADALM-PLUTO on USB (0456:B673)"))?;
        let number = info
            .interfaces()
            .find(|i| i.interface_string() == Some("IIO"))
            .map(|i| i.interface_number())
            // ADI's driver binds interface 5; the string is not always
            // reported on Windows.
            .unwrap_or(5);
        let device = info.open().map_err(|e| {
            err(format!(
                "cannot open the device ({e}). Is ADI's Pluto USB driver (WinUSB) installed?"
            ))
        })?;
        let interface = device.claim_interface(number).map_err(|e| {
            err(format!(
                "cannot claim the IIO interface {number} ({e}). Install ADI's Pluto USB driver, \
                 or use the network link (ip:192.168.2.1)"
            ))
        })?;
        let mut ins = Vec::new();
        let mut outs = Vec::new();
        if let Some(alt) = interface.descriptors().next() {
            for ep in alt.endpoints() {
                if ep.transfer_type() != nusb::transfer::EndpointType::Bulk {
                    continue;
                }
                match ep.direction() {
                    nusb::transfer::Direction::In => ins.push(ep.address()),
                    nusb::transfer::Direction::Out => outs.push(ep.address()),
                }
            }
        }
        let pairs: Vec<(u8, u8)> = ins.into_iter().zip(outs).collect();
        if pairs.len() < 2 {
            return Err(err(format!(
                "interface {number} has {} endpoint pairs; IIOD needs two",
                pairs.len()
            )));
        }
        let usb = UsbIio {
            interface: Arc::new(interface),
            number,
            pairs,
        };
        usb.vendor(REQ_RESET_PIPES, 0)?;
        Ok(usb)
    }

    fn vendor(&self, request: u8, value: u16) -> Result<()> {
        block_on(self.interface.control_out(ControlOut {
            control_type: ControlType::Vendor,
            recipient: Recipient::Interface,
            request,
            value,
            index: self.number as u16,
            data: &[],
        }))
        .into_result()
        .map_err(err)?;
        Ok(())
    }

    /// Open pipe `id` and return it as a byte stream. `transfer` is the size
    /// of each bulk IN request and `depth` how many are kept queued: small
    /// for the control pipe, large for samples.
    pub fn pipe(&self, id: usize, transfer: usize, depth: usize) -> Result<UsbLink> {
        let &(ep_in, ep_out) = self
            .pairs
            .get(id)
            .ok_or_else(|| err(format!("no pipe {id}")))?;
        self.vendor(REQ_OPEN_PIPE, id as u16)?;
        let mut rx = self.interface.bulk_in_queue(ep_in);
        // A multiple of the 512-byte high-speed packet.
        let transfer = transfer.next_multiple_of(512);
        for _ in 0..depth.max(1) {
            rx.submit(RequestBuffer::new(transfer));
        }
        Ok(UsbLink {
            interface: Arc::clone(&self.interface),
            ep_out,
            rx,
            transfer,
            pending: Vec::new(),
            pos: 0,
        })
    }
}

/// One pipe: bulk OUT for commands, a queue of bulk IN requests kept full
/// for replies and samples.
pub struct UsbLink {
    interface: Arc<nusb::Interface>,
    ep_out: u8,
    rx: Queue<RequestBuffer>,
    transfer: usize,
    /// The last completed IN transfer, partly handed out.
    pending: Vec<u8>,
    pos: usize,
}

impl Link for UsbLink {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        block_on(self.interface.bulk_out(self.ep_out, bytes.to_vec()))
            .into_result()
            .map_err(io::Error::other)?;
        Ok(())
    }

    fn read_some(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.pending.len() {
            let Completion { data, status } = block_on(self.rx.next_complete());
            status.map_err(io::Error::other)?;
            // Hand the spent buffer back so the queue stays full.
            let spent = std::mem::replace(&mut self.pending, data);
            self.pos = 0;
            if spent.capacity() >= self.transfer {
                self.rx.submit(RequestBuffer::reuse(spent, self.transfer));
            } else {
                self.rx.submit(RequestBuffer::new(self.transfer));
            }
        }
        let n = (self.pending.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.pending[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Drop for UsbLink {
    fn drop(&mut self) {
        self.rx.cancel_all();
    }
}
