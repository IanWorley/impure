//! Reads Xbox 360 disc images (XDVDFS), so the player can pick their own ISO and the runtime
//! copies the game's files out of it once.
//!
//! The game's file system sits in a partition at a fixed offset that depends on the disc format.
//! Its volume descriptor, 32 sectors in, names the root directory table. Each directory table
//! is a binary tree of entries, each with the offsets (in 4-byte units, 0 for none) of its left
//! and right subtrees, its first sector and size, its attributes and its name.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

const SECTOR_SIZE: u64 = 2048;
const MAGIC: &[u8] = b"MICROSOFT*XBOX*MEDIA";
const VOLUME_DESCRIPTOR_SECTOR: u64 = 32;
const VOLUME_ROOT_SECTOR: usize = 0x14;
const VOLUME_ROOT_SIZE: usize = 0x18;
/// Where the game partition starts: a rebuilt image (no video partition), XGD2 (most Xbox 360
/// games, Pure among them), XGD3, and original Xbox (XGD1) discs.
const PARTITION_OFFSETS: [u64; 4] = [0, 0x0FD9_0000, 0x0208_0000, 0x1830_0000];

const ENTRY_LEFT: usize = 0x0;
const ENTRY_RIGHT: usize = 0x2;
const ENTRY_SECTOR: usize = 0x4;
const ENTRY_SIZE: usize = 0x8;
const ENTRY_ATTRIBUTES: usize = 0xC;
const ENTRY_NAME_LENGTH: usize = 0xD;
const ENTRY_NAME: usize = 0xE;
/// Subtree offsets count 4-byte units from the start of the table.
const ENTRY_OFFSET_UNIT: usize = 4;
const NO_SUBTREE: u16 = 0;
/// Tables pad the rest of a sector with this byte; an offset of all ones points at padding.
const PADDING_OFFSET: u16 = 0xFFFF;
const ATTRIBUTE_DIRECTORY: u8 = 0x10;
const COPY_BUFFER_SIZE: usize = 1 << 20;

/// A file or directory on the disc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Relative to the disc's root.
    pub path: PathBuf,
    pub is_directory: bool,
    sector: u32,
    pub size: u32,
}

pub struct Image {
    file: File,
    partition: u64,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

impl Image {
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        for partition in PARTITION_OFFSETS {
            let mut magic = [0; MAGIC.len()];
            let found = file
                .seek(SeekFrom::Start(
                    partition + VOLUME_DESCRIPTOR_SECTOR * SECTOR_SIZE,
                ))
                .and_then(|_| file.read_exact(&mut magic));
            if found.is_ok() && magic == MAGIC {
                return Ok(Self { file, partition });
            }
        }
        Err(invalid(format!(
            "{} is not an Xbox disc image",
            path.display()
        )))
    }

    fn read_at(&mut self, sector: u32, length: usize) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0; length];
        self.file.seek(SeekFrom::Start(
            self.partition + u64::from(sector) * SECTOR_SIZE,
        ))?;
        self.file.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    /// Every file and directory, each directory before its contents.
    pub fn entries(&mut self) -> io::Result<Vec<Entry>> {
        let volume = self.read_at(VOLUME_DESCRIPTOR_SECTOR as u32, SECTOR_SIZE as usize)?;
        let root_sector = u32_at(&volume, VOLUME_ROOT_SECTOR);
        let root_size = u32_at(&volume, VOLUME_ROOT_SIZE);
        let mut entries = Vec::new();
        let mut directories = vec![(PathBuf::new(), root_sector, root_size)];
        while let Some((parent, sector, size)) = directories.pop() {
            for entry in self.directory(&parent, sector, size)? {
                if entry.is_directory {
                    directories.push((entry.path.clone(), entry.sector, entry.size));
                }
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    /// The entries of one directory table, walking its tree.
    fn directory(&mut self, parent: &Path, sector: u32, size: u32) -> io::Result<Vec<Entry>> {
        if size == 0 {
            return Ok(Vec::new());
        }
        let table = self.read_at(sector, size as usize)?;
        let mut entries = Vec::new();
        let mut pending = vec![0usize];
        let mut visited = std::collections::BTreeSet::new();
        while let Some(offset) = pending.pop() {
            if !visited.insert(offset) || offset + ENTRY_NAME > table.len() {
                return Err(invalid("a directory table points outside itself"));
            }
            let name_length = usize::from(table[offset + ENTRY_NAME_LENGTH]);
            let name_bytes = table
                .get(offset + ENTRY_NAME..offset + ENTRY_NAME + name_length)
                .ok_or_else(|| invalid("a directory entry's name runs past its table"))?;
            let name = String::from_utf8_lossy(name_bytes).into_owned();
            // Names come from the disc: one that is not a single plain component could write
            // outside the extraction directory.
            if !matches!(
                Path::new(&name).components().collect::<Vec<_>>()[..],
                [Component::Normal(_)]
            ) {
                return Err(invalid(format!("the disc has a file named {name:?}")));
            }
            entries.push(Entry {
                path: parent.join(&name),
                is_directory: table[offset + ENTRY_ATTRIBUTES] & ATTRIBUTE_DIRECTORY != 0,
                sector: u32_at(&table, offset + ENTRY_SECTOR),
                size: u32_at(&table, offset + ENTRY_SIZE),
            });
            for subtree in [ENTRY_LEFT, ENTRY_RIGHT] {
                let child =
                    u16::from_le_bytes([table[offset + subtree], table[offset + subtree + 1]]);
                if child != NO_SUBTREE && child != PADDING_OFFSET {
                    pending.push(usize::from(child) * ENTRY_OFFSET_UNIT);
                }
            }
        }
        Ok(entries)
    }

    /// Copies a file's contents to `output`, reporting each chunk's length.
    pub fn copy(
        &mut self,
        entry: &Entry,
        output: &mut impl Write,
        mut copied: impl FnMut(u64),
    ) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(
            self.partition + u64::from(entry.sector) * SECTOR_SIZE,
        ))?;
        let mut remaining = u64::from(entry.size);
        let mut buffer = vec![0; COPY_BUFFER_SIZE];
        while remaining > 0 {
            let chunk = remaining.min(buffer.len() as u64) as usize;
            self.file.read_exact(&mut buffer[..chunk])?;
            output.write_all(&buffer[..chunk])?;
            remaining -= chunk as u64;
            copied(chunk as u64);
        }
        Ok(())
    }

    /// A file's whole contents.
    pub fn read(&mut self, entry: &Entry) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(entry.size as usize);
        self.copy(entry, &mut bytes, |_| {})?;
        Ok(bytes)
    }

    /// Copies every file into `destination`, which must not exist yet, reporting bytes copied and
    /// the total. The files go to a sibling directory first, renamed into place once complete, so
    /// an interrupted extraction never looks finished.
    pub fn extract(
        &mut self,
        entries: &[Entry],
        destination: &Path,
        mut progress: impl FnMut(u64, u64),
    ) -> io::Result<()> {
        let partial = destination.with_extension("partial");
        if partial.exists() {
            fs::remove_dir_all(&partial)?;
        }
        fs::create_dir_all(&partial)?;
        let total: u64 = entries
            .iter()
            .filter(|entry| !entry.is_directory)
            .map(|entry| u64::from(entry.size))
            .sum();
        let mut done = 0;
        for entry in entries {
            let path = partial.join(&entry.path);
            if entry.is_directory {
                fs::create_dir_all(&path)?;
                continue;
            }
            let mut output = io::BufWriter::new(File::create(&path)?);
            self.copy(entry, &mut output, |chunk| {
                done += chunk;
                progress(done, total);
            })?;
            output.flush()?;
        }
        fs::rename(&partial, destination)
    }
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT_SECTOR: u32 = 33;
    const AUDIO_SECTOR: u32 = 34;
    const XEX_SECTOR: u32 = 35;
    const MUSIC_SECTOR: u32 = 36;
    const SECTOR_COUNT: u64 = 37;
    const XEX: &[u8] = b"xex!";
    const MUSIC: &[u8] = b"la la";

    /// One directory entry, padded to the 4-byte boundary the next must start on.
    fn entry(
        left: u16,
        right: u16,
        sector: u32,
        size: u32,
        directory: bool,
        name: &str,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(left.to_le_bytes());
        bytes.extend(right.to_le_bytes());
        bytes.extend(sector.to_le_bytes());
        bytes.extend(size.to_le_bytes());
        bytes.push(if directory { ATTRIBUTE_DIRECTORY } else { 0 });
        bytes.push(name.len() as u8);
        bytes.extend(name.as_bytes());
        while bytes.len() % ENTRY_OFFSET_UNIT != 0 {
            bytes.push(0);
        }
        bytes
    }

    fn put(image: &mut [u8], sector: u32, bytes: &[u8]) {
        let start = u64::from(sector) * SECTOR_SIZE;
        image[start as usize..start as usize + bytes.len()].copy_from_slice(bytes);
    }

    /// A rebuilt image: `Audio/music.bin` and `default.xex`, the root's tree having `Audio` at
    /// its top and `default.xex` as its right subtree.
    fn write_image(path: &Path) {
        let mut image = vec![0u8; (SECTOR_COUNT * SECTOR_SIZE) as usize];
        let mut volume = MAGIC.to_vec();
        volume.extend(ROOT_SECTOR.to_le_bytes());
        volume.extend((SECTOR_SIZE as u32).to_le_bytes());
        put(&mut image, VOLUME_DESCRIPTOR_SECTOR as u32, &volume);
        let audio = |right| entry(0, right, AUDIO_SECTOR, SECTOR_SIZE as u32, true, "Audio");
        // `default.xex` follows `Audio`, whose entry is the same length whatever it points to.
        let right = (audio(0).len() / ENTRY_OFFSET_UNIT) as u16;
        let mut root = audio(right);
        root.extend(entry(
            0,
            0,
            XEX_SECTOR,
            XEX.len() as u32,
            false,
            "default.xex",
        ));
        put(&mut image, ROOT_SECTOR, &root);
        put(
            &mut image,
            AUDIO_SECTOR,
            &entry(0, 0, MUSIC_SECTOR, MUSIC.len() as u32, false, "music.bin"),
        );
        put(&mut image, XEX_SECTOR, XEX);
        put(&mut image, MUSIC_SECTOR, MUSIC);
        fs::write(path, image).unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pure-runtime-xiso-{name}-{}", std::process::id()))
    }

    #[test]
    fn lists_the_tree_and_reads_files() {
        let path = scratch("list.iso");
        write_image(&path);
        let mut image = Image::open(&path).unwrap();
        let entries = image.entries().unwrap();
        let paths: Vec<_> = entries.iter().map(|entry| entry.path.clone()).collect();
        assert_eq!(
            paths,
            [
                PathBuf::from("Audio"),
                PathBuf::from("default.xex"),
                Path::new("Audio").join("music.bin")
            ]
        );
        let xex = entries
            .iter()
            .find(|entry| entry.path == Path::new("default.xex"))
            .unwrap();
        assert_eq!(image.read(xex).unwrap(), XEX);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn extracts_every_file_into_place() {
        let path = scratch("extract.iso");
        let destination = scratch("extract-disc");
        write_image(&path);
        let mut image = Image::open(&path).unwrap();
        let entries = image.entries().unwrap();
        let mut last = (0, 0);
        image
            .extract(&entries, &destination, |done, total| last = (done, total))
            .unwrap();
        assert_eq!(
            fs::read(destination.join("Audio/music.bin")).unwrap(),
            MUSIC
        );
        assert_eq!(
            last,
            (
                (XEX.len() + MUSIC.len()) as u64,
                (XEX.len() + MUSIC.len()) as u64
            )
        );
        fs::remove_file(path).unwrap();
        fs::remove_dir_all(destination).unwrap();
    }

    #[test]
    fn rejects_a_file_that_is_not_a_disc_image() {
        let path = scratch("not.iso");
        fs::write(&path, vec![0; (SECTOR_COUNT * SECTOR_SIZE) as usize]).unwrap();
        assert!(Image::open(&path).is_err());
        fs::remove_file(path).unwrap();
    }
}
