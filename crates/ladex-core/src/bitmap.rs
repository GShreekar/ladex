// A fixed-size set of chunk indices: which chunks of a file a node has.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bitmap {
    words: Vec<u64>,
    len: u32,
    ones: u32,
}

impl Bitmap {
    pub fn new(len: u32) -> Self {
        Self { words: vec![0; (len as usize).div_ceil(64)], len, ones: 0 }
    }

    pub fn full(len: u32) -> Self {
        let mut map = Self::new(len);
        for i in 0..len {
            map.set(i);
        }
        map
    }

    pub fn len(&self) -> u32 {
        self.len
    }

    pub fn count(&self) -> u32 {
        self.ones
    }

    pub fn is_full(&self) -> bool {
        self.ones == self.len
    }

    pub fn get(&self, index: u32) -> bool {
        index < self.len && self.words[(index / 64) as usize] & (1 << (index % 64)) != 0
    }

    // True if the chunk was not already present.
    pub fn set(&mut self, index: u32) -> bool {
        if index >= self.len || self.get(index) {
            return false;
        }
        self.words[(index / 64) as usize] |= 1 << (index % 64);
        self.ones += 1;
        true
    }

    // True if the chunk was present.
    pub fn clear(&mut self, index: u32) -> bool {
        if !self.get(index) {
            return false;
        }
        self.words[(index / 64) as usize] &= !(1 << (index % 64));
        self.ones -= 1;
        true
    }

    // How many chunks from the start are all present.
    pub fn leading_ones(&self) -> u32 {
        let mut index = 0;
        while index < self.len && self.get(index) {
            index += 1;
        }
        index
    }

    pub fn first_missing_from(&self, start: u32) -> Option<u32> {
        (start..self.len).find(|i| !self.get(*i))
    }

    pub fn iter_missing(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.len).filter(|i| !self.get(*i))
    }

    // Little-endian bit order inside each byte: chunk 0 is the lowest bit of byte 0.
    pub fn to_hex(&self) -> String {
        let bytes: Vec<u8> = (0..(self.len as usize).div_ceil(8))
            .map(|b| (0..8).fold(0u8, |acc, bit| acc | ((self.get((b * 8 + bit) as u32) as u8) << bit)))
            .collect();
        hex::encode(bytes)
    }

    // None if the text isn't hex or is the wrong length for `len` chunks.
    pub fn from_hex(len: u32, text: &str) -> Option<Self> {
        let bytes = hex::decode(text).ok()?;
        if bytes.len() != (len as usize).div_ceil(8) {
            return None;
        }
        let mut map = Self::new(len);
        for index in 0..len {
            if bytes[(index / 8) as usize] & (1 << (index % 8)) != 0 {
                map.set(index);
            }
        }
        Some(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_and_counting() {
        let mut map = Bitmap::new(130);
        assert_eq!((map.len(), map.count(), map.is_full()), (130, 0, false));
        assert!(map.set(0) && map.set(64) && map.set(129));
        assert!(!map.set(64), "setting twice reports no change");
        assert!(!map.set(130), "out of range is ignored");
        assert_eq!(map.count(), 3);
        assert!(map.get(129) && !map.get(128) && !map.get(500));
    }

    #[test]
    fn clearing() {
        let mut map = Bitmap::full(70);
        assert!(map.clear(65) && !map.clear(65));
        assert_eq!((map.count(), map.get(65), map.is_full()), (69, false, false));
        assert_eq!(map.first_missing_from(0), Some(65));
        assert!(!map.clear(500));
    }

    #[test]
    fn leading_and_missing() {
        let mut map = Bitmap::new(10);
        for i in [0, 1, 2, 4] {
            map.set(i);
        }
        assert_eq!(map.leading_ones(), 3);
        assert_eq!(map.first_missing_from(0), Some(3));
        assert_eq!(map.first_missing_from(4), Some(5));
        assert_eq!(map.iter_missing().collect::<Vec<_>>(), [3, 5, 6, 7, 8, 9]);
        assert_eq!(Bitmap::full(10).first_missing_from(0), None);
        assert!(Bitmap::full(10).is_full());
    }

    #[test]
    fn hex_round_trip_and_validation() {
        let mut map = Bitmap::new(11);
        for i in [0, 3, 8, 10] {
            map.set(i);
        }
        assert_eq!(map.to_hex(), "0905");
        assert_eq!(Bitmap::from_hex(11, "0905"), Some(map));
        assert_eq!(Bitmap::from_hex(11, "09"), None, "wrong length");
        assert_eq!(Bitmap::from_hex(11, "zz05"), None, "not hex");
        assert_eq!(Bitmap::new(0).to_hex(), "");
        assert_eq!(Bitmap::from_hex(0, ""), Some(Bitmap::new(0)));
    }
}
