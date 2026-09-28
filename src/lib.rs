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
use net::{Target, ceiling};
pub use session::{Event, Session};
use transport::error::{Result, protocol_error};
use transport::listening::{Accepting, Listening};
use transport::loopback::{FarEnd, LOOPBACK_TIMEOUT, Loopback};
use transport::socket;
use transport::{Arrived, Configured, Directions, Pool, Transport};
use xcore::settings::{Applies, Kind, Presence, Read, Setting, Settings};

/// What one S7 address spans: the Any pointer that names a variable counts
/// its bytes in sixteen bits, and a delivery is one write at one address.
const MAX_SPAN: usize = 65_535;

#[derive(Clone)]
pub struct S7Transport {
    plc: String,
    address: String,
    /// `address` parsed once, where it is an S7 address.
    parsed: Option<Address>,
    rack: u8,
    slot: u8,
    timeout: Option<Duration>,
    /// The sessions a send writes on and a receive polls on, set up once per
    /// CPU and kept.
    sessions: Pool<Client>,
}

impl S7Transport {
    /// Speak to the CPU at `plc` — `host:102` — about `address`, rack 0
    /// slot 2 until [`Self::at`] says otherwise.
    #[must_use]
    pub fn new(plc: impl Into<String>, address: impl Into<String>) -> Self {
        let address = address.into();
        Self {
            plc: plc.into(),
            parsed: address.parse().ok(),
            address,
            rack: 0,
            slot: 2,
            timeout: None,
            sessions: Pool::new(),
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
        match Target::under(&["s7comm"], target).map(|named| (named.authority(), named.path())) {
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

    /// One poll of the address, on the session kept for the CPU and set up
    /// on the first receive: its bytes as one Stream.
    fn receive(&self) -> Result<Vec<Arrived>> {
        // Parsed when the transport was made; parsed again only to say why
        // an address that is none is refused.
        let address = match self.parsed {
            Some(address) => address,
            None => self.address.parse()?,
        };
        let bytes = self.sessions.exchange(
            self.plc.as_str(),
            || self.connect(),
            |client| client.read(&address),
        )?;
        Ok(vec![Arrived::new(
            format!("s7comm://{}/{address}", self.plc),
            bytes,
        )])
    }

    /// Write `bytes` at the address; their length is the range written. On
    /// the session kept for the CPU, set up on the first send to it.
    fn send(&self, target: &str, bytes: &[u8]) -> Result<()> {
        let (plc, address) = self.resolve(target);
        let address: Address = address.parse()?;
        self.sessions.exchange(
            plc,
            || Client::connect(plc, self.rack, self.slot, self.timeout),
            |client| client.write(&address, bytes),
        )
    }
}

impl Configured for S7Transport {
    /// The address is the CPU, `host:102`: where a Location connects.
    const SETTINGS: &'static Settings = &Settings {
        technology: env!("CARGO_PKG_NAME"),
        settings: &[
            Setting {
                name: "variable",
                kind: Kind::Text,
                presence: Presence::Required,
                meaning: "The S7 address a Receive Location polls and a Send Location writes \
                          when a target names none, such as `DB12.DBB0.100`.",
                applies: Applies::Both,
            },
            Setting {
                name: "rack",
                kind: Kind::Integer {
                    minimum: 0,
                    maximum: 7,
                },
                presence: Presence::Optional,
                meaning: "The rack the CPU sits in; rack 0 when left out.",
                applies: Applies::Both,
            },
            Setting {
                name: "slot",
                kind: Kind::Integer {
                    minimum: 0,
                    maximum: 31,
                },
                presence: Presence::Optional,
                meaning: "The slot the CPU sits in, slot 1 for 1200 and 1500 series; slot 2 \
                          when left out.",
                applies: Applies::Both,
            },
            Setting {
                name: "timeout",
                kind: Kind::Duration,
                presence: Presence::Optional,
                meaning: "How long a CPU that stops answering is waited on; unbounded when left \
                          out.",
                applies: Applies::Both,
            },
        ],
    };

    /// The variable is refused here if it is no S7 address, rather than at
    /// the first poll.
    fn configured(address: &str, settings: &Read) -> Result<Self> {
        let variable = settings.text("variable");
        variable.parse::<Address>()?;
        let mut transport = Self::new(address, variable);
        let within = |name| {
            settings
                .optional_integer(name)
                .map(|n| u8::try_from(n).map_err(|_| protocol_error("out of range")))
                .transpose()
        };
        let rack = within("rack")?.unwrap_or(transport.rack);
        let slot = within("slot")?.unwrap_or(transport.slot);
        transport = transport.at(rack, slot);
        if let Some(timeout) = settings.optional_duration("timeout") {
            transport = transport.timing_out_after(timeout);
        }
        Ok(transport)
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
    fn take_one(self, listener: &TcpListener) -> Result<Arrived> {
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
        Ok(Box::new(Listening::new(self.clone(), self.bind()?)))
    }

    /// A transport of its own, gone once the write is: its kept session
    /// closes with it, which is how the far end knows the write is whole.
    fn send_to(&self, address: &str, payload: &[u8]) -> Result<()> {
        ceiling::within(payload.len(), MAX_SPAN, "one address spans")?;
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
    fn s7comm_declares_its_settings_and_reads_through_them() {
        use xcore::settings::Given;
        assert_eq!(S7Transport::SETTINGS.problems(), Vec::<String>::new());
        let given = [
            (
                "variable".to_string(),
                Given::Text("DB12.DBB0.100".to_string()),
            ),
            ("slot".to_string(), Given::Integer(1)),
            ("timeout".to_string(), Given::Text("2s".to_string())),
        ];
        let built = S7Transport::open("plc:102", Applies::Receive, &given).expect("configured");
        assert_eq!(built.address, "DB12.DBB0.100");
        assert_eq!((built.rack, built.slot), (0, 1));
        assert_eq!(built.timeout, Some(Duration::from_secs(2)));
        let Err(refused) = S7Transport::open("plc:102", Applies::Send, &[]) else {
            panic!("the variable is required");
        };
        assert!(refused.message.contains("\"variable\""), "{refused}");
        let nonsense = [("variable".to_string(), Given::Text("nowhere".to_string()))];
        assert!(S7Transport::open("plc:102", Applies::Send, &nonsense).is_err());
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
    fn a_thousand_writes_set_up_once_and_a_session_the_cpu_closed_is_replaced() {
        const SENDS: usize = 1000;
        let far_end = node("127.0.0.1:0", "M0").timing_out_after(Duration::from_secs(5));
        let (listener, address) = far_end.bind().expect("binding");
        let near = node(&address, "M0").timing_out_after(Duration::from_secs(5));
        let sending = near.clone();
        let sender = std::thread::spawn(move || {
            let began = std::time::Instant::now();
            for n in 0..SENDS {
                sending.send("M0", &[u8::try_from(n % 256).expect("a byte")])?;
            }
            let took = began.elapsed();
            // Generous for a debug build under load: a millisecond a write.
            assert!(took < Duration::from_millis(SENDS as u64), "{took:?}");
            sending.send("M0", b"!")
        });
        let serve = |session: &mut Session, writes: usize| {
            let (mut setups, mut written) = (0, 0);
            while written < writes {
                match session.next_event().expect("serving").expect("one") {
                    Event::Setup { .. } => setups += 1,
                    Event::Written { served: true, .. } => written += 1,
                    other => panic!("{other:?}"),
                }
            }
            setups
        };
        let mut session = far_end.accept_one(&listener).expect("accepting").with_area(
            Area::Flag,
            0,
            vec![0u8; 8],
        );
        // The connect and Setup Communication once, for every write.
        assert!(serve(&mut session, SENDS) <= 1);
        assert_eq!(session.area(Area::Flag, 0).expect("m")[0], 231);
        drop(session);
        let mut again = far_end
            .accept_one(&listener)
            .expect("a new session")
            .with_area(Area::Flag, 0, vec![0u8; 8]);
        serve(&mut again, 1);
        assert_eq!(again.area(Area::Flag, 0).expect("m")[0], b'!');
        sender.join().expect("thread").expect("sending");
        assert_eq!(near.sessions.opened(), 2);
    }

    #[test]
    fn a_thousand_receives_set_up_once_and_a_session_the_cpu_closed_is_replaced() {
        const RECEIVES: usize = 1000;
        let far_end = node("127.0.0.1:0", "M0").timing_out_after(Duration::from_secs(5));
        let (listener, address) = far_end.bind().expect("binding");
        let near = node(&address, "M0").timing_out_after(Duration::from_secs(5));
        let receiving = near.clone();
        let receiver = std::thread::spawn(move || {
            let began = std::time::Instant::now();
            for _ in 0..RECEIVES {
                assert_eq!(receiving.receive()?[0].bytes, [7]);
            }
            let took = began.elapsed();
            // Generous for a debug build under load: a millisecond a poll.
            assert!(took < Duration::from_millis(RECEIVES as u64), "{took:?}");
            receiving.receive()
        });
        let serve = |session: &mut Session, reads: usize| {
            let (mut setups, mut read) = (0, 0);
            while read < reads {
                match session.next_event().expect("serving").expect("one") {
                    Event::Setup { .. } => setups += 1,
                    Event::Read { served: true, .. } => read += 1,
                    other => panic!("{other:?}"),
                }
            }
            setups
        };
        let accept = || {
            far_end
                .accept_one(&listener)
                .expect("a session")
                .with_area(Area::Flag, 0, vec![7u8; 8])
        };
        // The connect and Setup Communication once, for every poll.
        let mut session = accept();
        assert!(serve(&mut session, RECEIVES) <= 1);
        drop(session);
        let mut again = accept();
        serve(&mut again, 1);
        assert_eq!(
            receiver.join().expect("thread").expect("polled")[0].bytes,
            [7]
        );
        assert_eq!(near.sessions.opened(), 2);
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
