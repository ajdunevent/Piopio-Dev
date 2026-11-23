//! The two protobuf messages of the legacy wire format, written by hand:
//! they are small, and this avoids a code generator in the build.
//!
//! SignalMessage: ratchet_key = 1, counter = 2, previous_counter = 3, ciphertext = 4.
//! PreKeySignalMessage: pre_key_id = 1, base_key = 2, identity_key = 3,
//! message = 4, registration_id = 5, signed_pre_key_id = 6.

use crate::Error;

#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    fn varint(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.0.push(byte);
                break;
            }
            self.0.push(byte | 0x80);
        }
    }

    pub fn uint(&mut self, field: u32, v: u32) {
        self.varint(u64::from(field) << 3);
        self.varint(u64::from(v));
    }

    pub fn bytes(&mut self, field: u32, v: &[u8]) {
        self.varint((u64::from(field) << 3) | 2);
        self.varint(v.len() as u64);
        self.0.extend_from_slice(v);
    }
}

/// Field values by number; unknown wire types are an error.
#[derive(Default, Debug)]
pub struct Fields {
    pub uints: Vec<(u32, u64)>,
    pub bytes: Vec<(u32, Vec<u8>)>,
}

impl Fields {
    pub fn uint(&self, field: u32) -> Option<u32> {
        self.uints
            .iter()
            .rev()
            .find(|(f, _)| *f == field)
            .and_then(|(_, v)| u32::try_from(*v).ok())
    }

    pub fn bytes(&self, field: u32) -> Option<&[u8]> {
        self.bytes
            .iter()
            .rev()
            .find(|(f, _)| *f == field)
            .map(|(_, v)| v.as_slice())
    }
}

fn read_varint(data: &[u8], pos: &mut usize) -> Result<u64, Error> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *data.get(*pos).ok_or(Error::Malformed)?;
        *pos += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(Error::Malformed)
}

pub fn parse(data: &[u8]) -> Result<Fields, Error> {
    let mut fields = Fields::default();
    let mut pos = 0;
    while pos < data.len() {
        let key = read_varint(data, &mut pos)?;
        let field = u32::try_from(key >> 3).map_err(|_| Error::Malformed)?;
        match key & 7 {
            0 => {
                let v = read_varint(data, &mut pos)?;
                fields.uints.push((field, v));
            }
            2 => {
                let len =
                    usize::try_from(read_varint(data, &mut pos)?).map_err(|_| Error::Malformed)?;
                let end = pos.checked_add(len).ok_or(Error::Malformed)?;
                let v = data.get(pos..end).ok_or(Error::Malformed)?;
                fields.bytes.push((field, v.to_vec()));
                pos = end;
            }
            _ => return Err(Error::Malformed),
        }
    }
    Ok(fields)
}
