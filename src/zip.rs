// A streaming zip writer, for downloading a folder as one file in browsers
// that can't save into a directory.
//
// Files are stored uncompressed and their sizes are known in advance, so the
// archive can be produced as the file chunks arrive, never held in memory, and
// its exact length is known before the first byte is sent (the download shows
// real progress). Only the CRC isn't known until a file has been read, so each
// entry carries it in a trailing "data descriptor". Entries and archives too
// large for the classic format use zip64.

use crc32fast::Hasher;

const LOCAL_HEADER: u32 = 0x0403_4b50;
const DATA_DESCRIPTOR: u32 = 0x0807_4b50;
const CENTRAL_HEADER: u32 = 0x0201_4b50;
const END_OF_CENTRAL_DIRECTORY: u32 = 0x0605_4b50;
const ZIP64_END_OF_CENTRAL_DIRECTORY: u32 = 0x0606_4b50;
const ZIP64_LOCATOR: u32 = 0x0706_4b50;
const ZIP64_EXTRA: u16 = 0x0001;
// Bit 3: sizes and CRC follow the data. Bit 11: names are UTF-8.
const FLAGS: u16 = 0x0808;
const NEEDS_ZIP64: u32 = u32::MAX;

pub const CLASSIC_LIMIT: u64 = u32::MAX as u64;

// MS-DOS date and time, as zip stores them.
pub fn dos_time(when: chrono::DateTime<chrono::Utc>) -> (u16, u16) {
    use chrono::{Datelike, Timelike};
    let year = (when.year().clamp(1980, 2107) - 1980) as u16;
    let date = (year << 9) | ((when.month() as u16) << 5) | when.day() as u16;
    let time = ((when.hour() as u16) << 11) | ((when.minute() as u16) << 5) | (when.second() / 2) as u16;
    (time, date)
}

struct Finished {
    name: Vec<u8>,
    crc: u32,
    size: u64,
    offset: u64,
    time: (u16, u16),
    zip64: bool,
}

struct Open {
    name: Vec<u8>,
    size: u64,
    written: u64,
    offset: u64,
    time: (u16, u16),
    zip64: bool,
    crc: Hasher,
}

pub struct ZipWriter {
    // Entries or archives at or above this size use zip64 (the real limit, except in tests).
    threshold: u64,
    offset: u64,
    open: Option<Open>,
    finished: Vec<Finished>,
}

impl ZipWriter {
    pub fn new() -> Self {
        Self::with_threshold(CLASSIC_LIMIT)
    }

    fn with_threshold(threshold: u64) -> Self {
        Self { threshold, offset: 0, open: None, finished: Vec::new() }
    }

    // The header that must be sent before this file's bytes.
    pub fn start_file(&mut self, name: &str, size: u64, time: (u16, u16)) -> Vec<u8> {
        assert!(self.open.is_none(), "the previous file was not finished");
        let zip64 = size >= self.threshold;
        let name = name.as_bytes().to_vec();
        let mut out = Vec::with_capacity(30 + name.len() + 20);
        out.extend(LOCAL_HEADER.to_le_bytes());
        out.extend((if zip64 { 45u16 } else { 20 }).to_le_bytes());
        out.extend(FLAGS.to_le_bytes());
        out.extend(0u16.to_le_bytes()); // stored
        out.extend(time.0.to_le_bytes());
        out.extend(time.1.to_le_bytes());
        out.extend(0u32.to_le_bytes()); // CRC: in the data descriptor
        let sizes = if zip64 { NEEDS_ZIP64 } else { 0 };
        out.extend(sizes.to_le_bytes());
        out.extend(sizes.to_le_bytes());
        out.extend((name.len() as u16).to_le_bytes());
        out.extend((if zip64 { 20u16 } else { 0 }).to_le_bytes());
        out.extend(&name);
        if zip64 {
            out.extend(ZIP64_EXTRA.to_le_bytes());
            out.extend(16u16.to_le_bytes());
            out.extend(0u64.to_le_bytes());
            out.extend(0u64.to_le_bytes());
        }
        self.open = Some(Open { name, size, written: 0, offset: self.offset, time, zip64, crc: Hasher::new() });
        self.offset += out.len() as u64;
        out
    }

    // Account for `data`, which the caller sends as the file's bytes.
    pub fn file_data(&mut self, data: &[u8]) {
        let open = self.open.as_mut().expect("no file is open");
        open.crc.update(data);
        open.written += data.len() as u64;
        self.offset += data.len() as u64;
    }

    // The data descriptor that closes the file.
    pub fn finish_file(&mut self) -> Vec<u8> {
        let open = self.open.take().expect("no file is open");
        assert_eq!(open.written, open.size, "the file was not the size it was announced as");
        let crc = open.crc.finalize();
        let mut out = Vec::with_capacity(24);
        out.extend(DATA_DESCRIPTOR.to_le_bytes());
        out.extend(crc.to_le_bytes());
        if open.zip64 {
            out.extend(open.size.to_le_bytes());
            out.extend(open.size.to_le_bytes());
        } else {
            out.extend((open.size as u32).to_le_bytes());
            out.extend((open.size as u32).to_le_bytes());
        }
        self.offset += out.len() as u64;
        self.finished.push(Finished { name: open.name, crc, size: open.size, offset: open.offset, time: open.time, zip64: open.zip64 });
        out
    }

    // The central directory and end records.
    pub fn finish(&mut self) -> Vec<u8> {
        assert!(self.open.is_none(), "a file was left open");
        let directory_start = self.offset;
        let mut out = Vec::new();
        for entry in &self.finished {
            let big_offset = entry.offset >= self.threshold;
            let mut extra = Vec::new();
            if entry.zip64 {
                extra.extend(entry.size.to_le_bytes()); // uncompressed
                extra.extend(entry.size.to_le_bytes()); // compressed
            }
            if big_offset {
                extra.extend(entry.offset.to_le_bytes());
            }
            out.extend(CENTRAL_HEADER.to_le_bytes());
            out.extend(0x032Du16.to_le_bytes()); // made by Unix, version 4.5
            out.extend((if entry.zip64 || big_offset { 45u16 } else { 20 }).to_le_bytes());
            out.extend(FLAGS.to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(entry.time.0.to_le_bytes());
            out.extend(entry.time.1.to_le_bytes());
            out.extend(entry.crc.to_le_bytes());
            let size = if entry.zip64 { NEEDS_ZIP64 } else { entry.size as u32 };
            out.extend(size.to_le_bytes());
            out.extend(size.to_le_bytes());
            out.extend((entry.name.len() as u16).to_le_bytes());
            out.extend((if extra.is_empty() { 0u16 } else { extra.len() as u16 + 4 }).to_le_bytes());
            out.extend(0u16.to_le_bytes()); // comment
            out.extend(0u16.to_le_bytes()); // disk
            out.extend(0u16.to_le_bytes()); // internal attributes
            out.extend((0o100644u32 << 16).to_le_bytes());
            out.extend((if big_offset { NEEDS_ZIP64 } else { entry.offset as u32 }).to_le_bytes());
            out.extend(&entry.name);
            if !extra.is_empty() {
                out.extend(ZIP64_EXTRA.to_le_bytes());
                out.extend((extra.len() as u16).to_le_bytes());
                out.extend(&extra);
            }
        }
        let directory_size = out.len() as u64;
        let count = self.finished.len() as u64;

        let needs_zip64 = count >= 0xFFFF || directory_start >= self.threshold || directory_size >= self.threshold;
        if needs_zip64 {
            let record_offset = directory_start + directory_size;
            out.extend(ZIP64_END_OF_CENTRAL_DIRECTORY.to_le_bytes());
            out.extend(44u64.to_le_bytes());
            out.extend(45u16.to_le_bytes());
            out.extend(45u16.to_le_bytes());
            out.extend(0u32.to_le_bytes());
            out.extend(0u32.to_le_bytes());
            out.extend(count.to_le_bytes());
            out.extend(count.to_le_bytes());
            out.extend(directory_size.to_le_bytes());
            out.extend(directory_start.to_le_bytes());
            out.extend(ZIP64_LOCATOR.to_le_bytes());
            out.extend(0u32.to_le_bytes());
            out.extend(record_offset.to_le_bytes());
            out.extend(1u32.to_le_bytes());
        }
        out.extend(END_OF_CENTRAL_DIRECTORY.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        let entries16 = if needs_zip64 { 0xFFFF } else { count as u16 };
        out.extend(entries16.to_le_bytes());
        out.extend(entries16.to_le_bytes());
        out.extend((if needs_zip64 { NEEDS_ZIP64 } else { directory_size as u32 }).to_le_bytes());
        out.extend((if needs_zip64 { NEEDS_ZIP64 } else { directory_start as u32 }).to_le_bytes());
        out.extend(0u16.to_le_bytes());
        self.offset += out.len() as u64;
        out
    }
}

// The exact size of the archive for these (name, size) entries, without producing it.
pub fn archive_length(entries: &[(String, u64)]) -> u64 {
    archive_length_with(entries, CLASSIC_LIMIT)
}

fn archive_length_with(entries: &[(String, u64)], threshold: u64) -> u64 {
    let mut writer = ZipWriter::with_threshold(threshold);
    let mut total = 0u64;
    for (name, size) in entries {
        total += writer.start_file(name, *size, (0, 0)).len() as u64;
        // Only the length matters, not the bytes: advance without hashing them.
        writer.open.as_mut().unwrap().written = *size;
        writer.offset += *size;
        total += *size;
        total += writer.finish_file().len() as u64;
    }
    total + writer.finish().len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(files: &[(&str, Vec<u8>)], threshold: u64) -> Vec<u8> {
        let mut writer = ZipWriter::with_threshold(threshold);
        let mut out = Vec::new();
        for (name, data) in files {
            out.extend(writer.start_file(name, data.len() as u64, dos_time(chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap())));
            for piece in data.chunks(7) {
                writer.file_data(piece);
                out.extend(piece);
            }
            out.extend(writer.finish_file());
        }
        out.extend(writer.finish());
        assert_eq!(writer.offset, out.len() as u64, "the writer's own count must match what it produced");
        out
    }

    fn files() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("a.txt", b"hello world".to_vec()),
            ("dir/empty.bin", Vec::new()),
            ("dir/sub/ünïcode 日本.dat", (0..=255u8).cycle().take(5000).collect()),
        ]
    }

    // Reads the archive back with the `zip` reader semantics of Python, via a small independent parser:
    // end record → central directory → local headers + data, checking every CRC.
    fn read_back(archive: &[u8]) -> Vec<(String, Vec<u8>)> {
        let u16_at = |i: usize| u16::from_le_bytes(archive[i..i + 2].try_into().unwrap()) as u64;
        let u32_at = |i: usize| u32::from_le_bytes(archive[i..i + 4].try_into().unwrap()) as u64;
        let u64_at = |i: usize| u64::from_le_bytes(archive[i..i + 8].try_into().unwrap());

        let eocd = archive.windows(4).rposition(|w| w == END_OF_CENTRAL_DIRECTORY.to_le_bytes()).expect("end record");
        let (mut count, mut size, mut start) = (u16_at(eocd + 10), u32_at(eocd + 12), u32_at(eocd + 16));
        if count == 0xFFFF || size == u32::MAX as u64 || start == u32::MAX as u64 {
            let locator = eocd - 20;
            assert_eq!(u32_at(locator), ZIP64_LOCATOR as u64);
            let record = u64_at(locator + 8) as usize;
            assert_eq!(u32_at(record), ZIP64_END_OF_CENTRAL_DIRECTORY as u64);
            count = u64_at(record + 32);
            size = u64_at(record + 40);
            start = u64_at(record + 48);
        }
        let has_zip64_records = eocd >= 20 && u32_at(eocd - 20) == ZIP64_LOCATOR as u64;
        assert_eq!(start + size, if has_zip64_records { (eocd - 20 - 56) as u64 } else { eocd as u64 });

        let mut at = start as usize;
        let mut result = Vec::new();
        for _ in 0..count {
            assert_eq!(u32_at(at), CENTRAL_HEADER as u64);
            let (crc, mut csize, mut usize_, name_len, extra_len, mut offset) =
                (u32_at(at + 16), u32_at(at + 20), u32_at(at + 24), u16_at(at + 28) as usize, u16_at(at + 30) as usize, u32_at(at + 42));
            let name = String::from_utf8(archive[at + 46..at + 46 + name_len].to_vec()).unwrap();
            let mut extra = &archive[at + 46 + name_len..at + 46 + name_len + extra_len];
            if !extra.is_empty() {
                assert_eq!(u16::from_le_bytes([extra[0], extra[1]]), ZIP64_EXTRA);
                extra = &extra[4..];
                let mut take = |wanted: bool| -> u64 {
                    if !wanted {
                        return 0;
                    }
                    let v = u64::from_le_bytes(extra[..8].try_into().unwrap());
                    extra = &extra[8..];
                    v
                };
                if usize_ == u32::MAX as u64 { usize_ = take(true); }
                if csize == u32::MAX as u64 { csize = take(true); }
                if offset == u32::MAX as u64 { offset = take(true); }
            }
            assert_eq!(csize, usize_, "stored");
            // The local header, then the bytes, then the descriptor.
            let local = offset as usize;
            assert_eq!(u32_at(local), LOCAL_HEADER as u64);
            let data_at = local + 30 + u16_at(local + 26) as usize + u16_at(local + 28) as usize;
            let data = archive[data_at..data_at + usize_ as usize].to_vec();
            assert_eq!(crc32fast::hash(&data) as u64, crc, "crc of {name}");
            assert_eq!(u32_at(data_at + data.len()), DATA_DESCRIPTOR as u64);
            assert_eq!(u32_at(data_at + data.len() + 4), crc);
            result.push((name, data));
            at += 46 + name_len + extra_len;
        }
        result
    }

    #[test]
    fn a_classic_archive_reads_back_exactly() {
        let archive = build(&files(), CLASSIC_LIMIT);
        let back = read_back(&archive);
        let expected: Vec<(String, Vec<u8>)> = files().into_iter().map(|(n, d)| (n.to_string(), d)).collect();
        assert_eq!(back, expected);
    }

    #[test]
    fn zip64_entries_and_directories_read_back_exactly() {
        // A tiny threshold exercises every zip64 path: entry sizes, offsets, and the end records.
        for threshold in [1, 100, 5000, 20_000] {
            let archive = build(&files(), threshold);
            let back = read_back(&archive);
            let expected: Vec<(String, Vec<u8>)> = files().into_iter().map(|(n, d)| (n.to_string(), d)).collect();
            assert_eq!(back, expected, "threshold {threshold}");
        }
    }

    #[test]
    fn the_announced_length_is_exact() {
        for threshold in [CLASSIC_LIMIT, 1, 100, 5000] {
            let listing: Vec<(String, u64)> = files().iter().map(|(n, d)| (n.to_string(), d.len() as u64)).collect();
            let archive = build(&files(), threshold);
            assert_eq!(archive_length_with(&listing, threshold), archive.len() as u64, "threshold {threshold}");
        }
        assert_eq!(archive_length(&[]), 22, "an empty archive is just the end record");
    }

    #[test]
    fn an_empty_archive_is_valid() {
        let archive = build(&[], CLASSIC_LIMIT);
        assert_eq!(archive.len(), 22);
        assert!(read_back(&archive).is_empty());
    }

    #[test]
    fn dos_time_encodes_as_zip_expects() {
        let when = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(); // 2023-11-14 22:13:20 UTC
        let (time, date) = dos_time(when);
        assert_eq!((date >> 9) + 1980, 2023);
        assert_eq!((date >> 5) & 0xF, 11);
        assert_eq!(date & 0x1F, 14);
        assert_eq!((time >> 11, (time >> 5) & 0x3F, (time & 0x1F) * 2), (22, 13, 20));
        // Dates before 1980 clamp rather than wrap.
        assert_eq!(dos_time(chrono::DateTime::from_timestamp(0, 0).unwrap()).1 >> 9, 0);
    }

    #[test]
    #[should_panic(expected = "announced")]
    fn a_file_of_the_wrong_length_is_caught() {
        let mut writer = ZipWriter::new();
        writer.start_file("a", 10, (0, 0));
        writer.file_data(b"short");
        writer.finish_file();
    }

    #[test]
    fn file_names_may_contain_anything_a_name_can() {
        let archive = build(&[("a b/c.txt", b"x".to_vec())], CLASSIC_LIMIT);
        assert_eq!(read_back(&archive)[0].0, "a b/c.txt");
    }
}
