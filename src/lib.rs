#![forbid(unsafe_code)]

//! Streams that are areas of a Siemens S7 PLC. One address — a byte range
//! of a data block, the inputs, the outputs or the flags — is one Stream:
//! reading it is a poll, writing it is a delivery.
//!
//! S7comm is the protocol every 300-, 400-, 1200- and 1500-series CPU speaks
//! to its programming device, over ISO transport on TCP port 102. It is a
//! job-and-answer protocol: Setup Communication settles the PDU length, then
//! Read Var and Write Var move bytes by any-pointer. A Receive Location
//! connects to the CPU and reads its address; a Send Location connects and
//! writes there. The carrier is `xmip-core-transport-cotp`; this crate is
//! the S7 header, the addressing, and a client that speaks them — with a
//! [`Session`] that is one client's worth of CPU for tests and the
//! playground.
//!
//! The origin URI names the CPU and the address:
//! `s7comm://host:102/DB12.DBB0.100`. A target is the same, or a bare
//! address on the configured CPU, or `host:port` for the configured address.

pub mod client;
pub mod header;
pub mod item;
pub mod session;

use std::net::TcpListener;
use std::time::Duration;

pub use client::Client;
pub use header::Message;
pub use item::{Address, Area};
pub use session::{Event, Session};
use transport::error::{Result, protocol_error};
use transport::listening::{Accepting, Listening};
use transport::loopback::{FarEnd, LOOPBACK_TIMEOUT, Loopback};
use transport::socket;
use transport::{Arrived, Directions, Transport};

/// What one S7 address spans: the Any pointer that names a variable counts
/// its bytes in sixteen bits, and a delivery is one write at one address.
pub const MAX_SPAN: usize = 65_535;

#[derive(Clone)]
pub struct S7Transport {
    plc: String,
    address: String,
    rack: u8,
    slot: u8,
    timeout: Option<Duration>,
}

impl S7Transport {
    /// Speak to the CPU at `plc` — `host:102` — about `address`, rack 0
    /// slot 2 until [`Self::at`] says otherwise.
    #[must_use]
    pub fn new(plc: impl Into<String>, address: impl Into<String>) -> Self {
        Self {
            plc: plc.into(),
            address: address.into(),
            rack: 0,
            slot: 2,
            timeout: None,
        }
    }

    /// Where in the rack the CPU sits; slot 1 for 1200 and 1500 series.
    #[must_use]
    pub const fn at(mut self, rack: u8, slot: u8) -> Self {
        self.rack = rack;
        self.slot = slot;
        self
    }

    /// Give up on a CPU that stops answering.
    #[must_use]
    pub const fn timing_out_after(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Connect to the CPU and set up communication.
    ///
    /// # Errors
    /// Where the CPU could not be reached or did not speak S7.
    pub fn connect(&self) -> Result<Client> {
        Client::connect(&self.plc, self.rack, self.slot, self.timeout)
    }

    /// Bind as the CPU clients connect to, at the configured address, and
    /// report the address actually assigned.
    ///
    /// # Errors
    /// Where the address is taken, malformed, or not permitted.
    pub fn bind(&self) -> Result<(TcpListener, String)> {
        socket::bind_tcp(&self.plc)
    }

    /// Accept one client on an already-bound listener.
    ///
    /// # Errors
    /// Where the connection could not be accepted or the handshake failed.
    pub fn accept_one(&self, listener: &TcpListener) -> Result<Session> {
        Session::accept(listener, self.timeout)
    }

    /// The CPU and address a target names: `s7comm://host:102/DB12.DBB0.100`
    /// in full, a bare address on the configured CPU, or `host:port` for the
    /// configured address.
    fn resolve<'a>(&'a self, target: &'a str) -> (&'a str, &'a str) {
        match socket::target("s7comm", target) {
            Some((plc, "")) => (plc, &self.address),
            Some(pair) => pair,
            None if target.contains(':') => (target, &self.address),
            None => (&self.plc, target),
        }
    }
}

impl Transport for S7Transport {
    fn name(&self) -> &'static str {
        "s7comm"
    }

    fn directions(&self) -> Directions {
        Directions::BOTH
    }

    /// One poll of the address: its bytes as one Stream.
    fn receive(&self) -> Result<Vec<Arrived>> {
        let address: Address = self.address.parse()?;
        let mut client = self.connect()?;
        let bytes = client.read(&address)?;
        client.disconnect()?;
        Ok(vec![Arrived::new(
            format!("s7comm://{}/{address}", self.plc),
            bytes,
        )])
    }

    /// Write `bytes` at the address; their length is the range written.
    fn send(&self, target: &str, bytes: &[u8]) -> Result<()> {
        let (plc, address) = self.resolve(target);
        let address: Address = address.parse()?;
        let mut client = Client::connect(plc, self.rack, self.slot, self.timeout)?;
        client.write(&address, bytes)?;
        client.disconnect()
    }
}

/// The address the loopback writes at: data block 1 from byte 0, the
/// payload's length as the range.
const LOOPBACK_BLOCK: &str = "DB1.DBB0";

impl S7Transport {
    /// Both ends on this machine: an ephemeral local port, one session's
    /// worth of CPU holding data block 1 at the far end, the loopback
    /// timeout on every read. The near end writes the payload there in as
    /// many jobs as the PDU length takes; the far end takes the block as far
    /// as it was written once the client disconnects. An empty payload is a
    /// write of no jobs and an empty block: Setup Communication, then DR.
    #[must_use]
    pub fn loopback() -> Self {
        Self::new("127.0.0.1:0", LOOPBACK_BLOCK).timing_out_after(LOOPBACK_TIMEOUT)
    }
}

impl Accepting for S7Transport {
    fn take_one(&self, listener: &TcpListener) -> Result<Arrived> {
        // The block is sized to what one address can span, because the far
        // end stands before the payload is known; what came back is the
        // block as far as the client's write jobs reached.
        let mut session =
            self.accept_one(listener)?
                .with_area(Area::DataBlock, 1, vec![0u8; MAX_SPAN]);
        let mut written = 0usize;
        while let Some(event) = session.next_event()? {
            if let Event::Written {
                address,
                served: true,
            } = event
                && address.area == Area::DataBlock
                && address.db == 1
            {
                let end = usize::try_from(address.offset).unwrap_or(usize::MAX);
                written = written.max(end.saturating_add(usize::from(address.length)));
            }
        }
        let block = session
            .area(Area::DataBlock, 1)
            .and_then(|block| block.get(..written))
            .ok_or_else(|| protocol_error("the data block went missing"))?
            .to_vec();
        Ok(Arrived::new(session.origin(), block))
    }
}

impl Loopback for S7Transport {
    fn ceiling(&self) -> Option<usize> {
        Some(MAX_SPAN)
    }

    fn far_end(&self) -> Result<Box<dyn FarEnd>> {
        let (listener, address) = self.bind()?;
        Ok(Box::new(Listening::new(self.clone(), listener, address)))
    }

    fn send_to(&self, address: &str, payload: &[u8]) -> Result<()> {
        if payload.len() > MAX_SPAN {
            return Err(protocol_error(format!(
                "{} bytes is over the {MAX_SPAN} one address spans",
                payload.len()
            )));
        }
        Self::new(address, LOOPBACK_BLOCK)
            .at(self.rack, self.slot)
            .timing_out_after(LOOPBACK_TIMEOUT)
            .send(address, payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use transport::payload::{edge_payloads, patterned};

    fn node(plc: &str, address: &str) -> S7Transport {
        S7Transport::new(plc, address).timing_out_after(Duration::from_secs(2))
    }

    #[test]
    fn the_loopback_writes_one_stream_into_a_data_block_and_takes_it() {
        let arrived = S7Transport::loopback().round(b"write var").expect("round");
        assert_eq!(arrived.bytes, b"write var");
        // The session's origin is the carrier's: the CR the client opened
        // with, TSAPs and all.
        assert!(arrived.origin_uri.starts_with("cotp://127.0.0.1:"));
        assert!(arrived.origin_uri.ends_with("?src-tsap=0100&dst-tsap=0102"));
        let long = patterned(3000);
        assert_eq!(
            S7Transport::loopback().round(&long).expect("long").bytes,
            long
        );
    }

    #[test]
    fn the_loopback_returns_the_edge_payloads_whole_up_to_the_span() {
        let transport = S7Transport::loopback();
        assert_eq!(transport.ceiling(), Some(MAX_SPAN));
        for (name, bytes) in edge_payloads() {
            assert!(transport.refuses(&bytes).is_none(), "{name}");
            let arrived = transport
                .round(&bytes)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(arrived.bytes, bytes, "{name}");
        }
        let brim = patterned(MAX_SPAN);
        assert_eq!(transport.round(&brim).expect("brim").bytes, brim);
        let over = vec![0u8; MAX_SPAN + 1];
        let refused = transport.round(&over).expect_err("over the span");
        assert!(refused.message.starts_with("send failed:"), "{refused}");
    }

    #[test]
    fn a_location_reads_and_writes_an_area_of_the_far_end() {
        let far_end = node("127.0.0.1:0", "DB12.DBB0.100");
        let (listener, address) = far_end.bind().expect("binding");
        let near = address.clone();
        let location = std::thread::spawn(move || {
            let arrived = node(&near, "DB12.DBB0.100").receive()?;
            node(&near, "M0").send(&format!("s7comm://{near}/Q0.2"), &[0x0F, 0xF0])?;
            node(&near, "M0").send("M4", b"flag")?;
            node(&near, "M0").send(&near, &[1])?;
            Ok::<_, transport::TransportError>(arrived)
        });
        let mut session = far_end.accept_one(&listener).expect("accepting").with_area(
            Area::DataBlock,
            12,
            (1..=100u8).collect::<Vec<_>>(),
        );
        assert!(session.origin().ends_with("?src-tsap=0100&dst-tsap=0102"));
        session.serve().expect("read");
        let mut outputs =
            far_end
                .accept_one(&listener)
                .expect("second")
                .with_area(Area::Output, 0, vec![0u8; 4]);
        outputs.serve().expect("write");
        assert_eq!(
            outputs.area(Area::Output, 0).expect("q"),
            &[0x0F, 0xF0, 0, 0]
        );
        let mut flags =
            far_end
                .accept_one(&listener)
                .expect("third")
                .with_area(Area::Flag, 0, vec![0u8; 8]);
        flags.serve().expect("write");
        assert_eq!(&flags.area(Area::Flag, 0).expect("m")[4..], b"flag");
        let mut flags =
            far_end
                .accept_one(&listener)
                .expect("fourth")
                .with_area(Area::Flag, 0, vec![0u8; 8]);
        flags.serve().expect("write");
        assert_eq!(flags.area(Area::Flag, 0).expect("m")[0], 1);
        let arrived = location.join().expect("thread").expect("location");
        assert_eq!(arrived.len(), 1);
        assert_eq!(
            arrived[0].origin_uri,
            format!("s7comm://{address}/DB12.DBB0.100")
        );
        assert_eq!(arrived[0].bytes, (1..=100u8).collect::<Vec<_>>());
    }

    #[test]
    fn a_far_end_that_does_not_speak_s7_is_a_permanent_error() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address").to_string();
        let far_end = std::thread::spawn(move || {
            // Answer the CR properly, then answer setup with a syslog line.
            let mut connection =
                cotp::Connection::accept(&listener, 10, Some(Duration::from_secs(2))).expect("cc");
            let job = connection.next_data().expect("job").expect("one");
            assert_eq!(job[0], 0x32);
            connection
                .send_data(b"<34>1 - - - - - - not S7")
                .expect("nonsense");
            std::thread::sleep(Duration::from_millis(200));
        });
        let error = node(&address, "DB1.DBB0.1").receive().expect_err("refused");
        assert!(!error.retryable, "{error}");
        far_end.join().expect("thread");
        assert!(node("127.0.0.1:1", "not an address").receive().is_err());
        assert!(node("127.0.0.1:1", "M0").claims().is_none());
        assert_eq!(node("127.0.0.1:1", "M0").name(), "s7comm");
    }
}
