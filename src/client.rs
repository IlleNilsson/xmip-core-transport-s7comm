//! Xmip's side of one S7 session: connect through ISO transport to the
//! CPU's TSAP, set up communication, then read and write areas by address.
//!
//! A read or write longer than the negotiated PDU is made in as many jobs as
//! it takes, each a slice of the address; the caller sees one area of bytes.

use std::time::Duration;

use cotp::Connection;
use transport::error::{Result, protocol_error};

use crate::header::{self, DEFAULT_PDU_LENGTH, Message, READ, Rosctr, SETUP, Setup, WRITE};
use crate::item::{self, Address};

/// The TSAP Xmip presents itself from: a programming device, connection 0.
pub const LOCAL_TSAP: [u8; 2] = [0x01, 0x00];

/// The TSAP a CPU listens on for `rack` and `slot`: `01` then the rack in
/// the top three bits and the slot in the low five — `0102` for rack 0,
/// slot 2, where a 300-series CPU sits.
#[must_use]
pub const fn cpu_tsap(rack: u8, slot: u8) -> [u8; 2] {
    [0x01, (rack << 5) | (slot & 0x1F)]
}

/// One session with a CPU.
pub struct Client {
    connection: Connection,
    pdu_length: u16,
    reference: u16,
}

impl Client {
    /// Connect to the CPU at `address` in `rack` and `slot` and set up
    /// communication.
    ///
    /// # Errors
    /// Where the CPU could not be reached, refused the TSAP, or did not answer
    /// Setup Communication.
    pub fn connect(address: &str, rack: u8, slot: u8, timeout: Option<Duration>) -> Result<Self> {
        let connection = Connection::connect(
            address,
            &LOCAL_TSAP,
            &cpu_tsap(rack, slot),
            cotp::tpdu::DEFAULT_SIZE_CODE,
            timeout,
        )?;
        let mut client = Self {
            connection,
            pdu_length: DEFAULT_PDU_LENGTH,
            reference: 0,
        };
        let answer = client.exchange(Setup::proposing(DEFAULT_PDU_LENGTH).encode(), Vec::new())?;
        if answer.function() != Some(SETUP) {
            return Err(protocol_error("the CPU did not answer Setup Communication"));
        }
        let granted = Setup::decode(&answer.parameter)?.pdu_length;
        client.pdu_length = granted.clamp(32, DEFAULT_PDU_LENGTH);
        Ok(client)
    }

    /// The PDU length the CPU granted.
    #[must_use]
    pub const fn pdu_length(&self) -> u16 {
        self.pdu_length
    }

    /// The bytes of `address`.
    ///
    /// # Errors
    /// Where the CPU refused the address — object does not exist, out of
    /// range — or went away.
    pub fn read(&mut self, address: &Address) -> Result<Vec<u8>> {
        // Header 12, parameter 2, data item header 4: what remains is bytes.
        let per_job = self.pdu_length.saturating_sub(18).max(1);
        let mut bytes = Vec::with_capacity(usize::from(address.length));
        for slice in slices(address, per_job) {
            let mut parameter = vec![READ, 1];
            parameter.extend_from_slice(&item::encode_item(&slice));
            let answer = self.exchange(parameter, Vec::new())?;
            if answer.function() != Some(READ) {
                return Err(protocol_error("the CPU did not answer Read Var"));
            }
            let (code, data, _) = item::decode_data(&answer.data)?;
            if let Some(refusal) = item::refusal(code, &slice) {
                return Err(refusal);
            }
            if data.len() != usize::from(slice.length) {
                return Err(protocol_error("the CPU answered fewer bytes than asked"));
            }
            bytes.extend_from_slice(data);
        }
        Ok(bytes)
    }

    /// Write `bytes` at `address`; its length is theirs.
    ///
    /// # Errors
    /// Where the CPU refused the address or went away.
    pub fn write(&mut self, address: &Address, bytes: &[u8]) -> Result<()> {
        // Header 10, parameter 14, data item header 4: what remains is bytes.
        let per_job = self.pdu_length.saturating_sub(28).max(1);
        let length = u16::try_from(bytes.len())
            .map_err(|_| protocol_error("more bytes than one address spans"))?;
        let whole = address.slice(address.offset, length);
        let mut at = 0usize;
        for slice in slices(&whole, per_job) {
            let chunk = &bytes[at..at + usize::from(slice.length)];
            at += chunk.len();
            let mut parameter = vec![WRITE, 1];
            parameter.extend_from_slice(&item::encode_item(&slice));
            let answer = self.exchange(parameter, item::encode_data(item::RETURN_OK, chunk))?;
            if answer.function() != Some(WRITE) {
                return Err(protocol_error("the CPU did not answer Write Var"));
            }
            let code = answer
                .data
                .first()
                .copied()
                .ok_or_else(|| protocol_error("a Write Var answer without a return code"))?;
            if let Some(refusal) = item::refusal(code, &slice) {
                return Err(refusal);
            }
        }
        Ok(())
    }

    /// End the session.
    ///
    /// # Errors
    /// Where the CPU had already gone.
    pub fn disconnect(self) -> Result<()> {
        self.connection.disconnect()
    }

    /// Send one job and take the answer to it.
    fn exchange(&mut self, parameter: Vec<u8>, data: Vec<u8>) -> Result<Message> {
        self.reference = self.reference.wrapping_add(1);
        let job = Message::job(self.reference, parameter, data);
        self.connection.send_data(&header::encode(&job))?;
        let answer = self
            .connection
            .next_data()?
            .ok_or_else(|| protocol_error("the CPU closed before answering"))?;
        let answer = header::decode(&answer)?;
        if answer.reference != self.reference {
            return Err(protocol_error("an answer to a different job"));
        }
        if answer.rosctr == Rosctr::Job {
            return Err(protocol_error("a job where an answer was due"));
        }
        if answer.error != 0 {
            return Err(protocol_error(format!(
                "the CPU answered error {:#06x}",
                answer.error
            )));
        }
        Ok(answer)
    }
}

/// `address` in slices of at most `per_job` bytes.
fn slices(address: &Address, per_job: u16) -> Vec<Address> {
    let mut out = Vec::new();
    let mut offset = address.offset;
    let mut remaining = address.length;
    while remaining > 0 {
        let take = remaining.min(per_job);
        out.push(address.slice(offset, take));
        offset += u32::from(take);
        remaining -= take;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cpu_tsap_names_rack_and_slot_and_a_read_is_sliced_to_the_pdu() {
        assert_eq!(cpu_tsap(0, 2), [0x01, 0x02]);
        assert_eq!(cpu_tsap(1, 3), [0x01, 0x23]);
        let address: Address = "DB1.DBB10.25".parse().expect("address");
        let sliced = slices(&address, 10);
        assert_eq!(sliced.len(), 3);
        assert_eq!(sliced[0].to_string(), "DB1.DBB10.10");
        assert_eq!(sliced[2].to_string(), "DB1.DBB30.5");
        assert_eq!(slices(&address, 100), vec![address]);
    }
}
