//! The fields of [`Message`](crate::Message)'s encoding.

use crate::Item;

/// The most items a message lists: more wouldn't fit a frame anyway.
const MAX_ITEMS: usize = 65_536;

#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    pub fn u16(&mut self, value: u16) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    pub fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    pub fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    pub fn raw(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    pub fn bytes(&mut self, bytes: &[u8]) {
        self.u32(bytes.len() as u32);
        self.raw(bytes);
    }

    pub fn str(&mut self, text: &str) {
        self.bytes(text.as_bytes());
    }

    pub fn optional_str(&mut self, text: Option<&str>) {
        match text {
            Some(text) => {
                self.u8(1);
                self.str(text);
            }
            None => self.u8(0),
        }
    }

    pub fn items(&mut self, items: &[Item]) {
        self.u32(items.len() as u32);
        for item in items {
            self.str(&item.name);
            self.u8(u8::from(item.dir));
        }
    }
}

pub struct Reader<'a>(pub &'a [u8]);

impl Reader<'_> {
    fn take(&mut self, len: usize) -> Option<&[u8]> {
        if self.0.len() < len {
            return None;
        }
        let (taken, rest) = self.0.split_at(len);
        self.0 = rest;
        Some(taken)
    }

    pub fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    pub fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    pub fn bool(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    pub fn u16(&mut self) -> Option<u16> {
        self.array().map(u16::from_le_bytes)
    }

    pub fn u32(&mut self) -> Option<u32> {
        self.array().map(u32::from_le_bytes)
    }

    pub fn u64(&mut self) -> Option<u64> {
        self.array().map(u64::from_le_bytes)
    }

    pub fn bytes(&mut self) -> Option<Vec<u8>> {
        let len = self.u32()? as usize;
        self.take(len).map(<[u8]>::to_vec)
    }

    pub fn str(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?).ok()
    }

    pub fn optional_str(&mut self) -> Option<Option<String>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.str()?)),
            _ => None,
        }
    }

    pub fn items(&mut self) -> Option<Vec<crate::Item>> {
        let count = self.u32()? as usize;
        if count > MAX_ITEMS {
            return None;
        }
        // Each item takes at least 5 bytes, which bounds what a lying count allocates.
        let mut items = Vec::with_capacity(count.min(self.0.len() / 5));
        for _ in 0..count {
            items.push(Item {
                name: self.str()?,
                dir: self.bool()?,
            });
        }
        Some(items)
    }
}
