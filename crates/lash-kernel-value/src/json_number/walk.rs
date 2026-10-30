use super::DecodeError;

fn malformed() -> DecodeError {
    rmp_serde::decode::Error::Syntax("malformed MessagePack value".into()).into()
}

struct Walk<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl Walk<'_> {
    fn take(&mut self, len: usize) -> Result<&[u8], DecodeError> {
        let end = self.position.checked_add(len).ok_or_else(malformed)?;
        let result = self.bytes.get(self.position..end).ok_or_else(malformed)?;
        self.position = end;
        Ok(result)
    }

    fn u8(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from(self.take(1)?[0]))
    }

    fn u16(&mut self) -> Result<u64, DecodeError> {
        let bytes: [u8; 2] = self.take(2)?.try_into().map_err(|_| malformed())?;
        Ok(u64::from(u16::from_be_bytes(bytes)))
    }

    fn u32(&mut self) -> Result<u64, DecodeError> {
        let bytes: [u8; 4] = self.take(4)?.try_into().map_err(|_| malformed())?;
        Ok(u64::from(u32::from_be_bytes(bytes)))
    }

    fn blob(&mut self, len: u64, ext: bool) -> Result<(), DecodeError> {
        let len = len.checked_add(u64::from(ext)).ok_or_else(malformed)?;
        self.take(usize::try_from(len).map_err(|_| malformed())?)?;
        Ok(())
    }

    fn value(&mut self) -> Result<u64, DecodeError> {
        let marker = self.take(1)?[0];
        match marker {
            0x00..=0x7f | 0xe0..=0xff | 0xc0 | 0xc2 | 0xc3 => Ok(0),
            0x80..=0x8f => Ok(u64::from(marker & 0x0f) * 2),
            0x90..=0x9f => Ok(u64::from(marker & 0x0f)),
            0xa0..=0xbf => {
                self.blob(u64::from(marker & 0x1f), false)?;
                Ok(0)
            }
            0xcc | 0xd0 => {
                self.take(1)?;
                Ok(0)
            }
            0xcd | 0xd1 => {
                self.take(2)?;
                Ok(0)
            }
            0xca | 0xce | 0xd2 => {
                self.take(4)?;
                Ok(0)
            }
            0xcb | 0xcf | 0xd3 => {
                self.take(8)?;
                Ok(0)
            }
            0xc4 | 0xd9 | 0xc7 => {
                let len = self.u8()?;
                self.blob(len, marker == 0xc7)?;
                Ok(0)
            }
            0xc5 | 0xda | 0xc8 => {
                let len = self.u16()?;
                self.blob(len, marker == 0xc8)?;
                Ok(0)
            }
            0xc6 | 0xdb | 0xc9 => {
                let len = self.u32()?;
                self.blob(len, marker == 0xc9)?;
                Ok(0)
            }
            0xd4..=0xd8 => {
                self.blob(1_u64 << (marker - 0xd4), true)?;
                Ok(0)
            }
            0xdc => self.u16(),
            0xdd => self.u32(),
            0xde => Ok(self.u16()? * 2),
            0xdf => Ok(self.u32()? * 2),
            0xc1 => Err(malformed()),
        }
    }
}

pub(super) fn validate(bytes: &[u8]) -> Result<(), DecodeError> {
    let mut walk = Walk { bytes, position: 0 };
    let mut pending = [0_u64; 1025];
    pending[0] = 1;
    let mut depth = 0;
    loop {
        if pending[depth] == 0 {
            if depth == 0 {
                break;
            }
            depth -= 1;
            continue;
        }
        pending[depth] -= 1;
        let children = walk.value()?;
        if children > 0 {
            if children > (bytes.len() - walk.position) as u64 || depth >= 1023 {
                return Err(malformed());
            }
            depth += 1;
            pending[depth] = children;
        }
    }
    if walk.position != bytes.len() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(())
}
