//! What a Read Var or Write Var names and carries: the address of an area in
//! the PLC, its any-pointer on the wire, and the data item that answers or
//! accompanies it with a return code.
//!
//! An address is written the way Siemens engineers write it — `DB12.DBB0.100`
//! is data block 12 from byte 0 for 100 bytes; `M0.10`, `I0.4` and `Q0.2`
//! are flags, inputs and outputs from byte 0 for 10, 4 and 2 bytes. The
//! any-pointer is twelve bytes: specification 0x12, length 10, syntax id
//! 0x10 (S7ANY), transport size 0x02 (byte), the count, the DB number, the
//! area, and the start address in bits.

use std::fmt;
use std::str::FromStr;

use transport::error::{Result, TransportError, protocol_error};

/// The return code that says the item was served.
pub const RETURN_OK: u8 = 0xFF;
/// The return code for a data block, or range, the PLC does not have.
pub const OBJECT_DOES_NOT_EXIST: u8 = 0x0A;
/// The return code for an address past the end of an area.
pub const OUT_OF_RANGE: u8 = 0x05;

/// A memory area of the PLC and its code in the any-pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Area {
    /// `DB`: a data block, 0x84.
    DataBlock,
    /// `I` or `E`: process inputs, 0x81.
    Input,
    /// `Q` or `A`: process outputs, 0x82.
    Output,
    /// `M`: flags, or merkers, 0x83.
    Flag,
}

impl Area {
    /// The area code on the wire.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::DataBlock => 0x84,
            Self::Input => 0x81,
            Self::Output => 0x82,
            Self::Flag => 0x83,
        }
    }

    /// From the code on the wire.
    ///
    /// # Errors
    /// An area this transport does not address.
    pub fn from_code(code: u8) -> Result<Self> {
        match code {
            0x84 => Ok(Self::DataBlock),
            0x81 => Ok(Self::Input),
            0x82 => Ok(Self::Output),
            0x83 => Ok(Self::Flag),
            other => Err(protocol_error(format!(
                "area {other:#04x} is not addressed"
            ))),
        }
    }
}

/// A byte range of one area: `DB12.DBB0.100`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Address {
    pub area: Area,
    /// The data block number; zero for the other areas.
    pub db: u16,
    /// The first byte.
    pub offset: u32,
    /// How many bytes.
    pub length: u16,
}

impl Address {
    /// The same area from `offset` for `length` bytes: a chunk of this one.
    #[must_use]
    pub const fn slice(self, offset: u32, length: u16) -> Self {
        Self {
            offset,
            length,
            ..self
        }
    }
}

impl FromStr for Address {
    type Err = TransportError;

    fn from_str(text: &str) -> Result<Self> {
        let bad = || protocol_error(format!("{text:?} is not an S7 address"));
        let mut parts = text.split('.');
        let head = parts.next().ok_or_else(bad)?.to_ascii_uppercase();
        let (area, db, offset) = if let Some(db) = head.strip_prefix("DB") {
            let db = db.parse().map_err(|_| bad())?;
            let byte = parts.next().ok_or_else(bad)?.to_ascii_uppercase();
            let offset = byte.strip_prefix("DBB").ok_or_else(bad)?;
            (Area::DataBlock, db, offset.parse().map_err(|_| bad())?)
        } else {
            let (letter, offset) = head.split_at(1.min(head.len()));
            let area = match letter {
                "M" => Area::Flag,
                "I" | "E" => Area::Input,
                "Q" | "A" => Area::Output,
                _ => return Err(bad()),
            };
            (area, 0, offset.parse().map_err(|_| bad())?)
        };
        let length = match parts.next() {
            Some(length) => length.parse().map_err(|_| bad())?,
            None => 1,
        };
        if parts.next().is_some() || length == 0 {
            return Err(bad());
        }
        Ok(Self {
            area,
            db,
            offset,
            length,
        })
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.area {
            Area::DataBlock => write!(f, "DB{}.DBB{}.{}", self.db, self.offset, self.length),
            Area::Input => write!(f, "I{}.{}", self.offset, self.length),
            Area::Output => write!(f, "Q{}.{}", self.offset, self.length),
            Area::Flag => write!(f, "M{}.{}", self.offset, self.length),
        }
    }
}

/// `address` as the twelve-byte any-pointer.
#[must_use]
pub fn encode_item(address: &Address) -> [u8; 12] {
    let bits = (address.offset * 8).to_be_bytes();
    let [count_hi, count_lo] = address.length.to_be_bytes();
    let [db_hi, db_lo] = address.db.to_be_bytes();
    [
        0x12,
        0x0A,
        0x10,
        0x02,
        count_hi,
        count_lo,
        db_hi,
        db_lo,
        address.area.code(),
        bits[1],
        bits[2],
        bits[3],
    ]
}

/// The address an any-pointer names.
///
/// # Errors
/// Not an S7ANY pointer to bytes of an area this transport addresses.
pub fn decode_item(item: &[u8]) -> Result<Address> {
    if item.len() < 12 || item[..3] != [0x12, 0x0A, 0x10] {
        return Err(protocol_error("an item that is not an S7ANY pointer"));
    }
    let transport_size = item[3];
    let count = u16::from_be_bytes([item[4], item[5]]);
    let length = match transport_size {
        0x02 | 0x03 | 0x09 => count,
        0x04 => count * 2,
        0x06 => count * 4,
        0x01 => return Err(protocol_error("a bit address; this transport reads bytes")),
        other => {
            return Err(protocol_error(format!(
                "transport size {other:#04x} is not addressed"
            )));
        }
    };
    let bits = u32::from_be_bytes([0, item[9], item[10], item[11]]);
    Ok(Address {
        area: Area::from_code(item[8])?,
        db: u16::from_be_bytes([item[6], item[7]]),
        offset: bits / 8,
        length,
    })
}

/// One data item as a Read Var answer or a Write Var request carries it:
/// return code, transport size 0x04 (length in bits), length, the bytes, and
/// a pad byte where the next item would otherwise start on an odd byte.
#[must_use]
pub fn encode_data(return_code: u8, bytes: &[u8]) -> Vec<u8> {
    if return_code != RETURN_OK {
        return vec![return_code, 0, 0, 0];
    }
    let bits = u16::try_from(bytes.len() * 8).unwrap_or(u16::MAX);
    let mut out = Vec::with_capacity(4 + bytes.len() + 1);
    out.extend_from_slice(&[return_code, 0x04]);
    out.extend_from_slice(&bits.to_be_bytes());
    out.extend_from_slice(bytes);
    if !bytes.len().is_multiple_of(2) {
        out.push(0);
    }
    out
}

/// The return code and bytes of one data item, and how many bytes of `data`
/// it took, pad included.
///
/// # Errors
/// A data item shorter than its header or its length.
pub fn decode_data(data: &[u8]) -> Result<(u8, &[u8], usize)> {
    if data.len() < 4 {
        return Err(protocol_error("a data item shorter than its header"));
    }
    let return_code = data[0];
    if return_code != RETURN_OK {
        return Ok((return_code, &[], 4));
    }
    let length = usize::from(u16::from_be_bytes([data[2], data[3]]));
    let bytes = match data[1] {
        0x04 | 0x05 => length / 8,
        _ => length,
    };
    let payload = data
        .get(4..4 + bytes)
        .ok_or_else(|| protocol_error("a data item shorter than its length"))?;
    let taken = 4 + bytes + usize::from(!bytes.is_multiple_of(2));
    Ok((return_code, payload, taken.min(data.len())))
}

/// The error a return code stands for, or `None` for success.
#[must_use]
pub fn refusal(return_code: u8, address: &Address) -> Option<TransportError> {
    match return_code {
        RETURN_OK => None,
        OBJECT_DOES_NOT_EXIST => Some(protocol_error(format!("{address}: object does not exist"))),
        OUT_OF_RANGE => Some(protocol_error(format!("{address}: out of range"))),
        other => Some(protocol_error(format!(
            "{address}: the PLC answered {other:#04x}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_reads_writes_and_round_trips_through_its_pointer() {
        let db: Address = "DB12.DBB0.100".parse().expect("db");
        assert_eq!(
            (db.area, db.db, db.offset, db.length),
            (Area::DataBlock, 12, 0, 100)
        );
        assert_eq!(db.to_string(), "DB12.DBB0.100");
        assert_eq!(
            encode_item(&db),
            [0x12, 0x0A, 0x10, 0x02, 0, 100, 0, 12, 0x84, 0, 0, 0]
        );
        for (text, area, shown) in [
            ("M0.10", Area::Flag, "M0.10"),
            ("I0.4", Area::Input, "I0.4"),
            ("Q0.2", Area::Output, "Q0.2"),
            ("e3", Area::Input, "I3.1"),
            ("a7.2", Area::Output, "Q7.2"),
        ] {
            let address: Address = text.parse().expect(text);
            assert_eq!(address.area, area);
            assert_eq!(address.to_string(), shown);
            assert_eq!(decode_item(&encode_item(&address)).expect("item"), address);
        }
        let far: Address = "DB1.DBB1000.2".parse().expect("far");
        assert_eq!(encode_item(&far)[9..], [0, 0x1F, 0x40], "8000 bits");
        assert_eq!(decode_item(&encode_item(&far)).expect("item"), far);
        assert_eq!(far.slice(1004, 1).to_string(), "DB1.DBB1004.1");
    }

    #[test]
    fn a_data_item_carries_its_return_code_and_pads_to_even() {
        let ok = encode_data(RETURN_OK, b"abc");
        assert_eq!(ok, [0xFF, 0x04, 0, 24, b'a', b'b', b'c', 0]);
        assert_eq!(decode_data(&ok).expect("ok"), (RETURN_OK, &b"abc"[..], 8));
        let missing = encode_data(OBJECT_DOES_NOT_EXIST, b"ignored");
        assert_eq!(missing, [0x0A, 0, 0, 0]);
        assert_eq!(decode_data(&missing).expect("no"), (0x0A, &[][..], 4));
        assert!(decode_data(&[0xFF, 0x04, 0, 80, 1]).is_err(), "short");
        let db = "DB1.DBB0.1".parse().expect("db");
        assert!(refusal(RETURN_OK, &db).is_none());
        assert!(
            refusal(OUT_OF_RANGE, &db)
                .expect("refused")
                .to_string()
                .contains("out of range")
        );
        assert!(!refusal(0x03, &db).expect("refused").retryable);
    }

    #[test]
    fn what_is_not_an_address_is_refused() {
        for bad in [
            "",
            "DB",
            "DBx.DBB0",
            "DB1.DBX0.1",
            "DB1.DBB0.0",
            "X0.1",
            "M0.1.2",
            "M",
        ] {
            assert!(bad.parse::<Address>().is_err(), "{bad}");
        }
        assert!(decode_item(&[0x12, 0x0A, 0x10, 0x01, 0, 1, 0, 1, 0x84, 0, 0, 0]).is_err());
        assert!(decode_item(&[0x12, 0x0A, 0x10, 0x02, 0, 1, 0, 1, 0x1C, 0, 0, 0]).is_err());
        assert!(decode_item(&[0x12, 0x0A]).is_err());
        assert!(Area::from_code(0x80).is_err());
    }
}
