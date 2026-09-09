//! The S7 protocol data unit: a ten-byte header — twelve on an answer — a
//! parameter part and a data part, and the Setup Communication parameter
//! that opens every session.
//!
//! `32 RR 00 00 PP PP LL LL DD DD [EE EE]`: protocol id 0x32, the remote
//! operating service control — job, ack, ack-data, user data — two reserved
//! bytes, the PDU reference the answer echoes, the parameter and data
//! lengths, and on an ack the error class and code. Siemens never published
//! this; what is here is what the plant floor reverse-engineered and every
//! open S7 driver agrees on.

use transport::error::{Result, protocol_error};

/// The first byte of every S7 PDU.
pub const PROTOCOL_ID: u8 = 0x32;
/// The function code of Setup Communication.
pub const SETUP: u8 = 0xF0;
/// The function code of Read Var.
pub const READ: u8 = 0x04;
/// The function code of Write Var.
pub const WRITE: u8 = 0x05;
/// The PDU length a client proposes: what a 300-series CPU grants.
pub const DEFAULT_PDU_LENGTH: u16 = 480;

/// Remote operating service control: what kind of PDU this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rosctr {
    /// A request.
    Job = 0x01,
    /// An answer without data.
    Ack = 0x02,
    /// An answer with data.
    AckData = 0x03,
    /// Diagnostics and the like; carried, not interpreted here.
    UserData = 0x07,
}

/// One PDU, header fields and both parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub rosctr: Rosctr,
    pub reference: u16,
    /// Error class and code; zero on a job and on success.
    pub error: u16,
    pub parameter: Vec<u8>,
    pub data: Vec<u8>,
}

impl Message {
    /// A job carrying `parameter` and `data` under `reference`.
    #[must_use]
    pub const fn job(reference: u16, parameter: Vec<u8>, data: Vec<u8>) -> Self {
        Self {
            rosctr: Rosctr::Job,
            reference,
            error: 0,
            parameter,
            data,
        }
    }

    /// The ack-data answering `reference`.
    #[must_use]
    pub const fn ack_data(reference: u16, parameter: Vec<u8>, data: Vec<u8>) -> Self {
        Self {
            rosctr: Rosctr::AckData,
            reference,
            error: 0,
            parameter,
            data,
        }
    }

    /// The function code the parameter part opens with, if any.
    #[must_use]
    pub fn function(&self) -> Option<u8> {
        self.parameter.first().copied()
    }
}

/// `message` as bytes on the wire.
#[must_use]
pub fn encode(message: &Message) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + message.parameter.len() + message.data.len());
    out.extend_from_slice(&[PROTOCOL_ID, message.rosctr as u8, 0, 0]);
    out.extend_from_slice(&message.reference.to_be_bytes());
    out.extend_from_slice(&length(&message.parameter).to_be_bytes());
    out.extend_from_slice(&length(&message.data).to_be_bytes());
    if matches!(message.rosctr, Rosctr::Ack | Rosctr::AckData) {
        out.extend_from_slice(&message.error.to_be_bytes());
    }
    out.extend_from_slice(&message.parameter);
    out.extend_from_slice(&message.data);
    out
}

fn length(part: &[u8]) -> u16 {
    u16::try_from(part.len()).unwrap_or(u16::MAX)
}

/// Read one PDU.
///
/// # Errors
/// Not S7: a protocol id other than 0x32, a ROSCTR nobody sends, or lengths
/// past the end.
pub fn decode(bytes: &[u8]) -> Result<Message> {
    if bytes.len() < 10 {
        return Err(protocol_error("an S7 PDU shorter than its header"));
    }
    if bytes[0] != PROTOCOL_ID {
        return Err(protocol_error(format!(
            "protocol id {:#04x} where S7 is 0x32",
            bytes[0]
        )));
    }
    let rosctr = match bytes[1] {
        0x01 => Rosctr::Job,
        0x02 => Rosctr::Ack,
        0x03 => Rosctr::AckData,
        0x07 => Rosctr::UserData,
        other => return Err(protocol_error(format!("ROSCTR {other:#04x} is not S7"))),
    };
    let reference = u16::from_be_bytes([bytes[4], bytes[5]]);
    let parameter_len = usize::from(u16::from_be_bytes([bytes[6], bytes[7]]));
    let data_len = usize::from(u16::from_be_bytes([bytes[8], bytes[9]]));
    let (error, header_len) = if matches!(rosctr, Rosctr::Ack | Rosctr::AckData) {
        let error = bytes
            .get(10..12)
            .map(|e| u16::from_be_bytes([e[0], e[1]]))
            .ok_or_else(|| protocol_error("an ack without its error field"))?;
        (error, 12)
    } else {
        (0, 10)
    };
    let parameter = bytes
        .get(header_len..header_len + parameter_len)
        .ok_or_else(|| protocol_error("a parameter length past the end"))?;
    let data = bytes
        .get(header_len + parameter_len..header_len + parameter_len + data_len)
        .ok_or_else(|| protocol_error("a data length past the end"))?;
    Ok(Message {
        rosctr,
        reference,
        error,
        parameter: parameter.to_vec(),
        data: data.to_vec(),
    })
}

/// The Setup Communication parameter, function 0xF0: how many jobs may be
/// outstanding each way, and the PDU length both sides settle on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    pub max_calling: u16,
    pub max_called: u16,
    pub pdu_length: u16,
}

impl Setup {
    /// One job at a time each way, `pdu_length` proposed.
    #[must_use]
    pub const fn proposing(pdu_length: u16) -> Self {
        Self {
            max_calling: 1,
            max_called: 1,
            pdu_length,
        }
    }

    /// The parameter part.
    #[must_use]
    pub fn encode(self) -> Vec<u8> {
        let mut out = vec![SETUP, 0];
        out.extend_from_slice(&self.max_calling.to_be_bytes());
        out.extend_from_slice(&self.max_called.to_be_bytes());
        out.extend_from_slice(&self.pdu_length.to_be_bytes());
        out
    }

    /// From a parameter part.
    ///
    /// # Errors
    /// A parameter that is not Setup Communication, or is short.
    pub fn decode(parameter: &[u8]) -> Result<Self> {
        if parameter.len() < 8 || parameter[0] != SETUP {
            return Err(protocol_error(
                "a parameter that is not Setup Communication",
            ));
        }
        Ok(Self {
            max_calling: u16::from_be_bytes([parameter[2], parameter[3]]),
            max_called: u16::from_be_bytes([parameter[4], parameter[5]]),
            pdu_length: u16::from_be_bytes([parameter[6], parameter[7]]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_and_its_answer_round_trip() {
        let job = Message::job(7, Setup::proposing(480).encode(), Vec::new());
        let bytes = encode(&job);
        assert_eq!(&bytes[..10], &[0x32, 1, 0, 0, 0, 7, 0, 8, 0, 0]);
        assert_eq!(&bytes[10..], &[0xF0, 0, 0, 1, 0, 1, 1, 0xE0]);
        assert_eq!(decode(&bytes).expect("job"), job);
        assert_eq!(job.function(), Some(SETUP));
        let mut answer = Message::ack_data(7, vec![READ, 1], vec![0xFF, 4, 0, 8, 1]);
        answer.error = 0x8104;
        let bytes = encode(&answer);
        assert_eq!(&bytes[..12], &[0x32, 3, 0, 0, 0, 7, 0, 2, 0, 5, 0x81, 0x04]);
        assert_eq!(decode(&bytes).expect("answer"), answer);
        assert_eq!(
            Setup::decode(&Setup::proposing(240).encode()).expect("setup"),
            Setup::proposing(240)
        );
    }

    #[test]
    fn what_is_not_s7_is_refused() {
        assert!(decode(&[0x32, 1, 0, 0, 0, 1, 0, 0, 0]).is_err(), "short");
        assert!(decode(&[0x33, 1, 0, 0, 0, 1, 0, 0, 0, 0]).is_err(), "id");
        assert!(
            decode(&[0x32, 9, 0, 0, 0, 1, 0, 0, 0, 0]).is_err(),
            "rosctr"
        );
        assert!(
            decode(&[0x32, 1, 0, 0, 0, 1, 0, 4, 0, 0]).is_err(),
            "past end"
        );
        assert!(
            decode(&[0x32, 3, 0, 0, 0, 1, 0, 0, 0, 0]).is_err(),
            "no error"
        );
        assert!(Setup::decode(&[READ, 1]).is_err());
        assert!(!decode(&[]).expect_err("empty").retryable);
    }
}
