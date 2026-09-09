//! The CPU's side of one session: what a test or the playground puts at the
//! far end so a Location can be driven without a PLC in the room.
//!
//! Not a PLC. One session serves one client and holds areas as byte
//! vectors keyed by area and data block number: it answers Setup
//! Communication, serves Read Var from an area and Write Var into one, and
//! refuses what is not there with the return codes a CPU would use.

use std::collections::HashMap;
use std::net::TcpListener;
use std::time::Duration;

use cotp::Connection;
use transport::error::{Result, protocol_error};

use crate::header::{self, Message, READ, Rosctr, SETUP, Setup, WRITE};
use crate::item::{self, Address, Area, OBJECT_DOES_NOT_EXIST, OUT_OF_RANGE, RETURN_OK};

/// What the client asked, as [`Session::next_event`] reports it after
/// answering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Setup Communication; the PDU length settled on.
    Setup { pdu_length: u16 },
    /// Read Var; the address, and whether it was served.
    Read { address: Address, served: bool },
    /// Write Var; the address, and whether it was served.
    Written { address: Address, served: bool },
}

pub struct Session {
    connection: Connection,
    areas: HashMap<(Area, u16), Vec<u8>>,
    pdu_length: u16,
}

impl Session {
    /// Accept one client on `listener`, answering its CR. Setup follows as
    /// the first event.
    ///
    /// # Errors
    /// Where the connection could not be accepted or did not open with CR.
    pub fn accept(listener: &TcpListener, timeout: Option<Duration>) -> Result<Self> {
        let connection = Connection::accept(listener, cotp::tpdu::DEFAULT_SIZE_CODE, timeout)?;
        Ok(Self {
            connection,
            areas: HashMap::new(),
            pdu_length: 240,
        })
    }

    /// Hold `bytes` as `area` — data block `db`, or zero for the others.
    #[must_use]
    pub fn with_area(mut self, area: Area, db: u16, bytes: impl Into<Vec<u8>>) -> Self {
        self.areas.insert((area, db), bytes.into());
        self
    }

    /// Grant at most `pdu_length` to a client's Setup Communication.
    #[must_use]
    pub const fn granting(mut self, pdu_length: u16) -> Self {
        self.pdu_length = pdu_length;
        self
    }

    /// The bytes held as `area`, as they are now.
    #[must_use]
    pub fn area(&self, area: Area, db: u16) -> Option<&[u8]> {
        self.areas.get(&(area, db)).map(Vec::as_slice)
    }

    /// The origin the client's requests come from.
    #[must_use]
    pub fn origin(&self) -> String {
        self.connection.origin()
    }

    /// Answer requests until the client disconnects.
    ///
    /// # Errors
    /// Where the connection broke or the client did not speak S7.
    pub fn serve(&mut self) -> Result<()> {
        while self.next_event()?.is_some() {}
        Ok(())
    }

    /// Answer the next request and say what it was, or `None` when the
    /// client disconnected.
    ///
    /// # Errors
    /// Where the connection broke, or the client sent what is not an S7 job.
    pub fn next_event(&mut self) -> Result<Option<Event>> {
        let Some(bytes) = self.connection.next_data()? else {
            return Ok(None);
        };
        let job = header::decode(&bytes)?;
        if job.rosctr != Rosctr::Job {
            return Err(protocol_error("an answer where a job was due"));
        }
        let (answer, event) = match job.function() {
            Some(SETUP) => self.setup(&job)?,
            Some(READ) => self.read(&job)?,
            Some(WRITE) => self.write(&job)?,
            other => {
                return Err(protocol_error(format!(
                    "function {other:?} is not one this session serves"
                )));
            }
        };
        self.connection.send_data(&header::encode(&answer))?;
        Ok(Some(event))
    }

    fn setup(&mut self, job: &Message) -> Result<(Message, Event)> {
        let asked = Setup::decode(&job.parameter)?;
        let pdu_length = asked.pdu_length.min(self.pdu_length);
        self.pdu_length = pdu_length;
        let granted = Setup {
            pdu_length,
            ..asked
        };
        Ok((
            Message::ack_data(job.reference, granted.encode(), Vec::new()),
            Event::Setup { pdu_length },
        ))
    }

    fn read(&self, job: &Message) -> Result<(Message, Event)> {
        let address = one_item(&job.parameter)?;
        let (code, bytes) = match self.areas.get(&(address.area, address.db)) {
            None => (OBJECT_DOES_NOT_EXIST, &[][..]),
            Some(area) => {
                let start = usize::try_from(address.offset).unwrap_or(usize::MAX);
                match area.get(start..start.saturating_add(usize::from(address.length))) {
                    Some(bytes) => (RETURN_OK, bytes),
                    None => (OUT_OF_RANGE, &[][..]),
                }
            }
        };
        Ok((
            Message::ack_data(job.reference, vec![READ, 1], item::encode_data(code, bytes)),
            Event::Read {
                address,
                served: code == RETURN_OK,
            },
        ))
    }

    fn write(&mut self, job: &Message) -> Result<(Message, Event)> {
        let address = one_item(&job.parameter)?;
        let (_, bytes, _) = item::decode_data(&job.data)?;
        let code = match self.areas.get_mut(&(address.area, address.db)) {
            None => OBJECT_DOES_NOT_EXIST,
            Some(area) => {
                let start = usize::try_from(address.offset).unwrap_or(usize::MAX);
                match area.get_mut(start..start.saturating_add(bytes.len())) {
                    Some(slot) => {
                        slot.copy_from_slice(bytes);
                        RETURN_OK
                    }
                    None => OUT_OF_RANGE,
                }
            }
        };
        Ok((
            Message::ack_data(job.reference, vec![WRITE, 1], vec![code]),
            Event::Written {
                address,
                served: code == RETURN_OK,
            },
        ))
    }
}

/// The one item a Read Var or Write Var from [`crate::Client`] names.
fn one_item(parameter: &[u8]) -> Result<Address> {
    if parameter.get(1) != Some(&1) {
        return Err(protocol_error("this session serves one item per job"));
    }
    item::decode_item(&parameter[2..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Client;

    #[test]
    fn a_client_sets_up_reads_and_writes_and_is_refused_what_is_not_there() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address").to_string();
        let client = std::thread::spawn(move || {
            let mut client = Client::connect(&address, 0, 2, Some(secs(2))).expect("connect");
            assert_eq!(client.pdu_length(), 240);
            let block: Address = "DB12.DBB0.100".parse().expect("address");
            let read = client.read(&block).expect("read");
            assert_eq!(read.len(), 100);
            assert_eq!(&read[..4], &[0, 1, 2, 3]);
            let long: Vec<u8> = (0..1000u32)
                .map(|n| u8::try_from(n % 256).expect("fits"))
                .collect();
            client
                .write(&"DB12.DBB200".parse().expect("a"), &long)
                .expect("write");
            assert_eq!(
                client
                    .read(&"DB12.DBB200.1000".parse().expect("a"))
                    .expect("back"),
                long
            );
            client
                .write(&"M0".parse().expect("m"), &[0xAA, 0x55])
                .expect("flags");
            let missing = client
                .read(&"DB99.DBB0.1".parse().expect("a"))
                .expect_err("no");
            assert!(missing.to_string().contains("object does not exist"));
            let past = client.read(&"I0.10".parse().expect("a")).expect_err("no");
            assert!(past.to_string().contains("out of range"));
            client.disconnect().expect("dr");
        });
        let mut session = Session::accept(&listener, Some(secs(2)))
            .expect("accept")
            .with_area(
                Area::DataBlock,
                12,
                (0..255u8).cycle().take(2000).collect::<Vec<_>>(),
            )
            .with_area(Area::Flag, 0, vec![0u8; 16])
            .with_area(Area::Input, 0, vec![0u8; 4]);
        assert_eq!(
            session.next_event().expect("setup"),
            Some(Event::Setup { pdu_length: 240 })
        );
        session.serve().expect("serving");
        assert_eq!(&session.area(Area::Flag, 0).expect("m")[..2], &[0xAA, 0x55]);
        assert_eq!(session.area(Area::DataBlock, 12).expect("db")[200], 0);
        assert_eq!(session.area(Area::DataBlock, 12).expect("db")[201], 1);
        client.join().expect("thread");
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }
}
