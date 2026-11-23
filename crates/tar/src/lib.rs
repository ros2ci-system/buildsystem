// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! A tar archive reader and writer.
//!
//! The writer produces ustar archives, with a pax extended header for any
//! path longer than 100 bytes. The reader understands ustar, pax extended
//! headers and the GNU long-name extension, which covers what Docker,
//! Podman and GNU tar produce.

use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

const BLOCK: usize = 512;

/// The largest pax or GNU long-name record the reader accepts. Real ones
/// are a few hundred bytes; the limit stops a hostile archive from asking
/// for an unbounded allocation.
const MAX_EXTENSION_SIZE: u64 = 1024 * 1024;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The archive is malformed.
    Invalid(&'static str),
    /// An entry's path would land outside the directory being unpacked to.
    UnsafePath(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "tar I/O error: {e}"),
            Error::Invalid(message) => write!(f, "invalid tar archive: {message}"),
            Error::UnsafePath(path) => write!(f, "tar entry has an unsafe path: {path}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
    Symlink,
    /// Hard links, devices, FIFOs and anything else.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The path as stored, with `/` separators.
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    pub mode: u32,
    /// The target of a symlink or hard link.
    pub link: String,
}

// ---------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------

pub struct Builder<W: Write> {
    out: W,
}

impl<W: Write> Builder<W> {
    pub fn new(out: W) -> Builder<W> {
        Builder { out }
    }

    /// Adds a regular file whose contents are already in memory.
    pub fn append_file(&mut self, path: &str, mode: u32, data: &[u8]) -> Result<(), Error> {
        self.append_reader(path, mode, data.len() as u64, &mut &data[..])
    }

    /// Adds a regular file of exactly `size` bytes read from `data`.
    pub fn append_reader(
        &mut self,
        path: &str,
        mode: u32,
        size: u64,
        data: &mut dyn Read,
    ) -> Result<(), Error> {
        self.header(path, b'0', mode, size)?;
        let copied = io::copy(&mut data.take(size), &mut self.out)?;
        if copied != size {
            return Err(Error::Invalid("file data was shorter than its stated size"));
        }
        self.pad(size)
    }

    /// Adds a directory.
    pub fn append_dir(&mut self, path: &str, mode: u32) -> Result<(), Error> {
        let with_slash = if path.ends_with('/') {
            path.to_string()
        } else {
            format!("{path}/")
        };
        self.header(&with_slash, b'5', mode, 0)
    }

    /// Adds everything under a directory on disk, stored under `prefix`.
    /// Only regular files and directories are added; other kinds of file
    /// are skipped. Entries are added in name order, so the same tree
    /// always produces the same archive.
    pub fn append_tree(&mut self, dir: &Path, prefix: &str) -> Result<(), Error> {
        let mut names: Vec<_> = fs::read_dir(dir)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|e| e.file_name())
            .collect();
        names.sort();
        for name in names {
            let Some(name_str) = name.to_str() else {
                return Err(Error::Invalid("file name is not valid UTF-8"));
            };
            let on_disk = dir.join(&name);
            let stored = if prefix.is_empty() {
                name_str.to_string()
            } else {
                format!("{}/{}", prefix.trim_end_matches('/'), name_str)
            };
            let meta = fs::symlink_metadata(&on_disk)?;
            if meta.is_dir() {
                self.append_dir(&stored, 0o755)?;
                self.append_tree(&on_disk, &stored)?;
            } else if meta.is_file() {
                let mut file = fs::File::open(&on_disk)?;
                self.append_reader(&stored, file_mode(&meta), meta.len(), &mut file)?;
            }
        }
        Ok(())
    }

    /// Writes the end-of-archive marker and returns the underlying writer.
    pub fn finish(mut self) -> Result<W, Error> {
        self.out.write_all(&[0u8; BLOCK * 2])?;
        self.out.flush()?;
        Ok(self.out)
    }

    fn header(&mut self, path: &str, kind: u8, mode: u32, size: u64) -> Result<(), Error> {
        if path.is_empty() {
            return Err(Error::Invalid("empty path"));
        }
        let name = if path.len() > 100 {
            // A pax record carries the real path; the ustar name field
            // gets a truncated copy for readers that ignore pax.
            let record = pax_record("path", path);
            let block = ustar_block("PaxHeader", b'x', 0o644, record.len() as u64);
            self.out.write_all(&block)?;
            self.out.write_all(record.as_bytes())?;
            self.pad(record.len() as u64)?;
            let mut cut = 100;
            while !path.is_char_boundary(cut) {
                cut -= 1;
            }
            &path[..cut]
        } else {
            path
        };
        self.out.write_all(&ustar_block(name, kind, mode, size))?;
        Ok(())
    }

    fn pad(&mut self, size: u64) -> Result<(), Error> {
        let rem = (size % BLOCK as u64) as usize;
        if rem != 0 {
            self.out.write_all(&[0u8; BLOCK][..BLOCK - rem])?;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn file_mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn file_mode(_meta: &fs::Metadata) -> u32 {
    0o644
}

/// A pax record is "<length> <key>=<value>\n", where the length counts the
/// whole record including its own digits.
fn pax_record(key: &str, value: &str) -> String {
    let body = key.len() + value.len() + 3; // space, '=' and newline
    let mut len = body + 1;
    loop {
        let total = body + len.to_string().len();
        if total == len {
            break;
        }
        len = total;
    }
    format!("{len} {key}={value}\n")
}

fn ustar_block(name: &str, kind: u8, mode: u32, size: u64) -> [u8; BLOCK] {
    let mut block = [0u8; BLOCK];
    block[..name.len()].copy_from_slice(name.as_bytes());
    octal(&mut block[100..108], u64::from(mode & 0o7777));
    octal(&mut block[108..116], 0); // uid
    octal(&mut block[116..124], 0); // gid
    if size < 0o100_000_000_000 {
        octal(&mut block[124..136], size);
    } else {
        // Sizes of 8 GiB and above use the base-256 extension.
        block[124] = 0x80;
        block[128..136].copy_from_slice(&size.to_be_bytes());
    }
    octal(&mut block[136..148], 0); // modification time: fixed, for reproducible archives
    block[156] = kind;
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    // The checksum is computed with its own field filled with spaces.
    block[148..156].copy_from_slice(b"        ");
    let sum: u32 = block.iter().map(|&b| u32::from(b)).sum();
    let text = format!("{sum:06o}\0 ");
    block[148..156].copy_from_slice(text.as_bytes());
    block
}

fn octal(field: &mut [u8], value: u64) {
    let width = field.len() - 1;
    let text = format!("{value:0width$o}");
    field[..width].copy_from_slice(&text.as_bytes()[text.len() - width..]);
    field[width] = 0;
}

// ---------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------

pub struct Archive<R: Read> {
    input: R,
    /// Bytes of the current entry's data not yet read.
    remaining: u64,
    /// Padding after the current entry's data.
    padding: u64,
    done: bool,
}

impl<R: Read> Archive<R> {
    pub fn new(input: R) -> Archive<R> {
        Archive {
            input,
            remaining: 0,
            padding: 0,
            done: false,
        }
    }

    /// Moves to the next entry, skipping whatever is left of the current
    /// one. Returns `None` at the end of the archive.
    pub fn next_entry(&mut self) -> Result<Option<Entry>, Error> {
        if self.done {
            return Ok(None);
        }
        let mut long_path: Option<String> = None;
        let mut long_link: Option<String> = None;
        let mut pax_size: Option<u64> = None;
        loop {
            self.skip_current()?;
            let mut block = [0u8; BLOCK];
            if !read_block(&mut self.input, &mut block)? {
                // Some writers leave off the end marker.
                self.done = true;
                return Ok(None);
            }
            if block.iter().all(|&b| b == 0) {
                self.done = true;
                // Drain the rest so the writer on the other end of a pipe
                // or socket is not left blocked.
                io::copy(&mut self.input, &mut io::sink())?;
                return Ok(None);
            }
            verify_checksum(&block)?;
            let size = parse_size(&block[124..136])?;
            let kind_byte = block[156];
            self.remaining = size;
            self.padding = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;

            match kind_byte {
                b'x' => {
                    let data = self.read_extension(size)?;
                    for (key, value) in parse_pax(&data)? {
                        match key.as_str() {
                            "path" => long_path = Some(value),
                            "linkpath" => long_link = Some(value),
                            "size" => {
                                pax_size = Some(
                                    value.parse().map_err(|_| Error::Invalid("bad pax size"))?,
                                )
                            }
                            _ => {}
                        }
                    }
                }
                // Global pax headers carry nothing we use.
                b'g' => {}
                b'L' => long_path = Some(c_string(&self.read_extension(size)?)?),
                b'K' => long_link = Some(c_string(&self.read_extension(size)?)?),
                _ => {
                    if let Some(size) = pax_size {
                        self.remaining = size;
                        self.padding = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
                    }
                    let path = match long_path {
                        Some(path) => path,
                        None => {
                            let name = c_string(&block[0..100])?;
                            let is_ustar = &block[257..262] == b"ustar";
                            let prefix = if is_ustar {
                                c_string(&block[345..500])?
                            } else {
                                String::new()
                            };
                            if prefix.is_empty() {
                                name
                            } else {
                                format!("{prefix}/{name}")
                            }
                        }
                    };
                    let link = match long_link {
                        Some(link) => link,
                        None => c_string(&block[157..257])?,
                    };
                    let kind = match kind_byte {
                        b'0' | 0 | b'7' => {
                            // Very old archives mark a directory only by a
                            // trailing slash.
                            if path.ends_with('/') {
                                Kind::Directory
                            } else {
                                Kind::File
                            }
                        }
                        b'5' => Kind::Directory,
                        b'2' => Kind::Symlink,
                        _ => Kind::Other,
                    };
                    return Ok(Some(Entry {
                        path,
                        kind,
                        size: self.remaining,
                        mode: (parse_octal(&block[100..108])? & 0o7777) as u32,
                        link,
                    }));
                }
            }
        }
    }

    /// Unpacks the rest of the archive into `dest`. Only regular files and
    /// directories are created; symlinks and other kinds are skipped, so
    /// nothing in the archive can make a later entry land outside `dest`.
    /// Returns the paths of the files written, relative to `dest`.
    pub fn unpack(&mut self, dest: &Path) -> Result<Vec<PathBuf>, Error> {
        let mut written = Vec::new();
        while let Some(entry) = self.next_entry()? {
            let Some(relative) = safe_relative_path(&entry.path)? else {
                continue;
            };
            let target = dest.join(&relative);
            match entry.kind {
                Kind::Directory => fs::create_dir_all(&target)?,
                Kind::File => {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let mut file = fs::File::create(&target)?;
                    io::copy(self, &mut file)?;
                    set_mode(&file, entry.mode)?;
                    written.push(relative);
                }
                Kind::Symlink | Kind::Other => {}
            }
        }
        Ok(written)
    }

    fn skip_current(&mut self) -> Result<(), Error> {
        let to_skip = self.remaining + self.padding;
        if to_skip > 0 {
            let skipped = io::copy(&mut (&mut self.input).take(to_skip), &mut io::sink())?;
            if skipped != to_skip {
                return Err(Error::Invalid("archive ends in the middle of an entry"));
            }
        }
        self.remaining = 0;
        self.padding = 0;
        Ok(())
    }

    fn read_extension(&mut self, size: u64) -> Result<Vec<u8>, Error> {
        if size > MAX_EXTENSION_SIZE {
            return Err(Error::Invalid("extension header is too large"));
        }
        let mut data = vec![0u8; size as usize];
        self.input
            .read_exact(&mut data)
            .map_err(|_| Error::Invalid("archive ends in the middle of an entry"))?;
        self.remaining = 0;
        Ok(data)
    }
}

/// Reads the data of the current entry.
impl<R: Read> Read for Archive<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = buf
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let n = self.input.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "archive ends in the middle of an entry",
            ));
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Turns a stored path into a relative path that cannot leave the
/// directory it is joined to. Returns `None` for a path that names the
/// directory itself.
fn safe_relative_path(stored: &str) -> Result<Option<PathBuf>, Error> {
    // Windows containers store paths with either separator.
    let normalized = stored.replace('\\', "/");
    let mut out = PathBuf::new();
    for part in normalized.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(Error::UnsafePath(stored.to_string())),
            part => {
                // A drive letter or an alternate data stream.
                if part.contains(':') {
                    return Err(Error::UnsafePath(stored.to_string()));
                }
                out.push(part);
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Ok(None);
    }
    // Defence in depth: the result must be made only of plain names.
    if !out.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(Error::UnsafePath(stored.to_string()));
    }
    Ok(Some(out))
}

#[cfg(unix)]
fn set_mode(file: &fs::File, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // Never create set-uid, set-gid or sticky files.
    file.set_permissions(fs::Permissions::from_mode(mode & 0o777))
}

#[cfg(not(unix))]
fn set_mode(_file: &fs::File, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// Fills `block` from the input. Returns `false` at a clean end of input,
/// before any byte of the block.
fn read_block(input: &mut impl Read, block: &mut [u8; BLOCK]) -> Result<bool, Error> {
    let mut filled = 0;
    while filled < BLOCK {
        match input.read(&mut block[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(false);
                }
                return Err(Error::Invalid("archive ends in the middle of a header"));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Ok(true)
}

fn verify_checksum(block: &[u8; BLOCK]) -> Result<(), Error> {
    let stored = parse_octal(&block[148..156])?;
    let sum: u64 = block
        .iter()
        .enumerate()
        .map(|(i, &b)| {
            if (148..156).contains(&i) {
                32
            } else {
                u64::from(b)
            }
        })
        .sum();
    if sum == stored {
        Ok(())
    } else {
        Err(Error::Invalid("header checksum does not match"))
    }
}

fn parse_octal(field: &[u8]) -> Result<u64, Error> {
    let mut value: u64 = 0;
    let mut seen_digit = false;
    for &b in field {
        match b {
            b'0'..=b'7' => {
                seen_digit = true;
                value = value
                    .checked_mul(8)
                    .and_then(|v| v.checked_add(u64::from(b - b'0')))
                    .ok_or(Error::Invalid("number too large"))?;
            }
            b' ' if !seen_digit => {}
            b' ' | 0 => break,
            _ => return Err(Error::Invalid("bad octal number in header")),
        }
    }
    Ok(value)
}

fn parse_size(field: &[u8]) -> Result<u64, Error> {
    if field[0] & 0x80 != 0 {
        // Base-256: the value is big-endian in the remaining bits.
        let mut value: u64 = 0;
        for (i, &b) in field.iter().enumerate() {
            let byte = if i == 0 { b & 0x7f } else { b };
            if value >> 56 != 0 {
                return Err(Error::Invalid("number too large"));
            }
            value = (value << 8) | u64::from(byte);
        }
        Ok(value)
    } else {
        parse_octal(field)
    }
}

fn c_string(field: &[u8]) -> Result<String, Error> {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8(field[..end].to_vec()).map_err(|_| Error::Invalid("path is not valid UTF-8"))
}

fn parse_pax(mut data: &[u8]) -> Result<Vec<(String, String)>, Error> {
    let bad = Error::Invalid("bad pax record");
    let mut records = Vec::new();
    while !data.is_empty() {
        let space = data
            .iter()
            .position(|&b| b == b' ')
            .ok_or(Error::Invalid("bad pax record"))?;
        let len: usize = std::str::from_utf8(&data[..space])
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or(Error::Invalid("bad pax record"))?;
        if len <= space + 1 || len > data.len() || data[len - 1] != b'\n' {
            return Err(bad);
        }
        let body = &data[space + 1..len - 1];
        let eq = body
            .iter()
            .position(|&b| b == b'=')
            .ok_or(Error::Invalid("bad pax record"))?;
        let key =
            String::from_utf8(body[..eq].to_vec()).map_err(|_| Error::Invalid("bad pax record"))?;
        let value = String::from_utf8(body[eq + 1..].to_vec())
            .map_err(|_| Error::Invalid("path is not valid UTF-8"))?;
        records.push((key, value));
        data = &data[len..];
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(f: impl FnOnce(&mut Builder<Vec<u8>>)) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        f(&mut builder);
        builder.finish().unwrap()
    }

    fn entries(bytes: &[u8]) -> Vec<(Entry, Vec<u8>)> {
        let mut archive = Archive::new(bytes);
        let mut out = Vec::new();
        while let Some(entry) = archive.next_entry().unwrap() {
            let mut data = Vec::new();
            archive.read_to_end(&mut data).unwrap();
            out.push((entry, data));
        }
        out
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ci_tar_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip() {
        let bytes = build(|b| {
            b.append_dir("job", 0o755).unwrap();
            b.append_file("job/params.json", 0o644, b"{}").unwrap();
            b.append_file("job/empty", 0o600, b"").unwrap();
            b.append_file("job/block", 0o755, &[7u8; 512]).unwrap();
            b.append_file("job/big", 0o644, &[9u8; 1300]).unwrap();
        });
        assert_eq!(bytes.len() % BLOCK, 0);
        let got = entries(&bytes);
        assert_eq!(got.len(), 5);
        assert_eq!(got[0].0.path, "job/");
        assert_eq!(got[0].0.kind, Kind::Directory);
        assert_eq!(got[1].0.path, "job/params.json");
        assert_eq!(got[1].0.kind, Kind::File);
        assert_eq!(got[1].0.mode, 0o644);
        assert_eq!(got[1].1, b"{}");
        assert_eq!(got[2].1, b"");
        assert_eq!(got[3].0.mode, 0o755);
        assert_eq!(got[3].1, vec![7u8; 512]);
        assert_eq!(got[4].1, vec![9u8; 1300]);
    }

    #[test]
    fn long_paths_use_pax() {
        let long = format!("{}/file-é.txt", "d".repeat(60).repeat(3));
        assert!(long.len() > 100);
        let bytes = build(|b| b.append_file(&long, 0o644, b"x").unwrap());
        let got = entries(&bytes);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.path, long);
        assert_eq!(got[0].1, b"x");
    }

    #[test]
    fn pax_record_lengths() {
        assert_eq!(pax_record("path", "a"), "9 path=a\n");
        assert_eq!(pax_record("path", "ab"), "11 path=ab\n");
        // Every length must be self-consistent, including where the
        // number of digits in the length changes.
        for n in 0..1200 {
            let value = "v".repeat(n);
            let record = pax_record("path", &value);
            let stated: usize = record.split(' ').next().unwrap().parse().unwrap();
            assert_eq!(stated, record.len(), "value length {n}");
            let parsed = parse_pax(record.as_bytes()).unwrap();
            assert_eq!(parsed, vec![("path".to_string(), value)]);
        }
    }

    #[test]
    fn skipping_unread_data() {
        let bytes = build(|b| {
            b.append_file("a", 0o644, &[1u8; 700]).unwrap();
            b.append_file("b", 0o644, b"second").unwrap();
        });
        let mut archive = Archive::new(&bytes[..]);
        assert_eq!(archive.next_entry().unwrap().unwrap().path, "a");
        // Read only part of "a", then move on.
        let mut partial = [0u8; 10];
        archive.read_exact(&mut partial).unwrap();
        let entry = archive.next_entry().unwrap().unwrap();
        assert_eq!(entry.path, "b");
        let mut data = Vec::new();
        archive.read_to_end(&mut data).unwrap();
        assert_eq!(data, b"second");
        assert!(archive.next_entry().unwrap().is_none());
        assert!(archive.next_entry().unwrap().is_none());
    }

    #[test]
    fn gnu_long_name_and_symlink() {
        // Built by hand: a GNU 'L' entry, then a symlink that it names.
        let long = "x".repeat(150);
        let mut bytes = Vec::new();
        let mut name_data = long.clone().into_bytes();
        name_data.push(0);
        bytes.extend_from_slice(&ustar_block(
            "././@LongLink",
            b'L',
            0o644,
            name_data.len() as u64,
        ));
        bytes.extend_from_slice(&name_data);
        bytes.resize(bytes.len().div_ceil(BLOCK) * BLOCK, 0);
        let mut link = ustar_block("short", b'2', 0o777, 0);
        link[157..163].copy_from_slice(b"target");
        link[148..156].copy_from_slice(b"        ");
        let sum: u32 = link.iter().map(|&b| u32::from(b)).sum();
        link[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        bytes.extend_from_slice(&link);
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);

        let got = entries(&bytes);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.path, long);
        assert_eq!(got[0].0.kind, Kind::Symlink);
        assert_eq!(got[0].0.link, "target");
    }

    #[test]
    fn ustar_prefix_is_joined() {
        let mut block = ustar_block("name.txt", b'0', 0o644, 0);
        block[345..352].copy_from_slice(b"some/di");
        block[148..156].copy_from_slice(b"        ");
        let sum: u32 = block.iter().map(|&b| u32::from(b)).sum();
        block[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        let mut bytes = block.to_vec();
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        assert_eq!(entries(&bytes)[0].0.path, "some/di/name.txt");
    }

    #[test]
    fn rejects_corruption() {
        let good = build(|b| b.append_file("a", 0o644, b"data").unwrap());

        let mut bad_sum = good.clone();
        bad_sum[0] = b'b';
        assert!(matches!(
            Archive::new(&bad_sum[..]).next_entry(),
            Err(Error::Invalid("header checksum does not match"))
        ));

        // Cut off in the middle of the data.
        let truncated = &good[..BLOCK + 2];
        let mut archive = Archive::new(truncated);
        archive.next_entry().unwrap().unwrap();
        assert!(archive.next_entry().is_err());

        // Cut off in the middle of a header.
        assert!(Archive::new(&good[..100]).next_entry().is_err());
    }

    #[test]
    fn missing_end_marker_is_accepted() {
        let good = build(|b| b.append_file("a", 0o644, b"data").unwrap());
        let got = entries(&good[..BLOCK * 2]);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn large_sizes_use_base_256() {
        let size = 9 * 1024 * 1024 * 1024u64;
        let block = ustar_block("big", b'0', 0o644, size);
        assert_eq!(parse_size(&block[124..136]).unwrap(), size);
        verify_checksum(&block).unwrap();
    }

    #[test]
    fn safe_paths() {
        assert_eq!(
            safe_relative_path("a/b.txt").unwrap(),
            Some(PathBuf::from("a/b.txt"))
        );
        assert_eq!(
            safe_relative_path("./a//b/").unwrap(),
            Some(PathBuf::from("a/b"))
        );
        assert_eq!(
            safe_relative_path("/abs/x").unwrap(),
            Some(PathBuf::from("abs/x"))
        );
        assert_eq!(
            safe_relative_path("a\\b").unwrap(),
            Some(PathBuf::from("a/b"))
        );
        assert_eq!(safe_relative_path("./").unwrap(), None);
        assert!(safe_relative_path("../x").is_err());
        assert!(safe_relative_path("a/../../x").is_err());
        assert!(safe_relative_path("a\\..\\x").is_err());
        assert!(safe_relative_path("C:/x").is_err());
        assert!(safe_relative_path("a/file:stream").is_err());
    }

    #[test]
    fn unpack_writes_files_and_refuses_escapes() {
        let dir = temp_dir("unpack");
        let bytes = build(|b| {
            b.append_dir("out", 0o755).unwrap();
            b.append_file("out/results.json", 0o644, b"{\"ok\":true}")
                .unwrap();
            b.append_file("out/sub/deep.txt", 0o644, b"deep").unwrap();
        });
        let written = Archive::new(&bytes[..]).unpack(&dir).unwrap();
        assert_eq!(
            written,
            vec![
                PathBuf::from("out/results.json"),
                PathBuf::from("out/sub/deep.txt")
            ]
        );
        assert_eq!(
            fs::read(dir.join("out/results.json")).unwrap(),
            b"{\"ok\":true}"
        );
        assert_eq!(fs::read(dir.join("out/sub/deep.txt")).unwrap(), b"deep");

        let evil = build(|b| b.append_file("../escaped", 0o644, b"x").unwrap());
        assert!(matches!(
            Archive::new(&evil[..]).unpack(&dir),
            Err(Error::UnsafePath(_))
        ));
        assert!(!dir.parent().unwrap().join("escaped").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unpack_skips_symlinks() {
        let dir = temp_dir("symlink");
        let mut link = ustar_block("out/link", b'2', 0o777, 0);
        link[157..168].copy_from_slice(b"/etc/passwd");
        link[148..156].copy_from_slice(b"        ");
        let sum: u32 = link.iter().map(|&b| u32::from(b)).sum();
        link[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        let mut bytes = link.to_vec();
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        let written = Archive::new(&bytes[..]).unpack(&dir).unwrap();
        assert!(written.is_empty());
        assert!(fs::symlink_metadata(dir.join("out/link")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn append_tree_is_sorted_and_round_trips() {
        let src = temp_dir("tree_src");
        fs::create_dir_all(src.join("b/inner")).unwrap();
        fs::write(src.join("z.txt"), b"z").unwrap();
        fs::write(src.join("a.txt"), b"a").unwrap();
        fs::write(src.join("b/inner/c.txt"), b"c").unwrap();
        let bytes = build(|b| b.append_tree(&src, "in").unwrap());
        let paths: Vec<String> = entries(&bytes).into_iter().map(|(e, _)| e.path).collect();
        assert_eq!(
            paths,
            [
                "in/a.txt",
                "in/b/",
                "in/b/inner/",
                "in/b/inner/c.txt",
                "in/z.txt"
            ]
        );
        fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn readable_by_system_tar_format() {
        // The magic, version and checksum layout that other tools check.
        let block = ustar_block("f", b'0', 0o644, 3);
        assert_eq!(&block[257..265], b"ustar\x0000");
        assert_eq!(&block[124..136], b"00000000003\0");
        assert_eq!(&block[100..108], b"0000644\0");
        assert_eq!(block[154], 0);
        assert_eq!(block[155], b' ');
    }
}
