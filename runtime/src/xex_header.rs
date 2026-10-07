//! Reads optional fields from the XEX2 header, which the loader copies into guest memory as the
//! console does, so kernel imports and guest code see the same bytes.

use crate::memory::GuestMemory;

/// Offset of the header's total size (which is also where the image data starts in the file).
const HEADER_SIZE_OFFSET: usize = 0x8;
const OPTIONAL_HEADER_COUNT_OFFSET: u32 = 0x14;
const OPTIONAL_HEADERS_OFFSET: u32 = 0x18;
/// Each directory entry is a key followed by a value.
const OPTIONAL_HEADER_ENTRY_SIZE: u32 = 8;
const OPTIONAL_HEADER_VALUE_OFFSET: u32 = 4;
/// A key's low byte says what its value holds.
const KEY_KIND_MASK: u32 = 0xFF;
const KIND_INLINE_VALUE: u32 = 0x00;
const KIND_INLINE_STRUCTURE: u32 = 0x01;

/// The image's system flags; each set bit grants an executable privilege.
pub const SYSTEM_FLAGS_KEY: u32 = 0x0003_0000;
/// The stack size for threads created without one, and for the main thread.
pub const DEFAULT_STACK_SIZE_KEY: u32 = 0x0002_0200;

/// The header at the start of an executable file, or None if the file is too short to hold it.
pub fn header_bytes(xex: &[u8]) -> Option<&[u8]> {
    let size = xex.get(HEADER_SIZE_OFFSET..HEADER_SIZE_OFFSET + size_of::<u32>())?;
    xex.get(..u32::from_be_bytes(size.try_into().unwrap()) as usize)
}

/// RtlImageXexHeaderField: an inline value is returned as is; anything else is returned as the
/// guest address of its data.
pub fn field(memory: &GuestMemory, header: u32, key: u32) -> Option<u32> {
    let count = memory.read_u32(header + OPTIONAL_HEADER_COUNT_OFFSET);
    (0..count)
        .map(|index| header + OPTIONAL_HEADERS_OFFSET + index * OPTIONAL_HEADER_ENTRY_SIZE)
        .find(|&entry| memory.read_u32(entry) == key)
        .map(|entry| {
            let value_address = entry + OPTIONAL_HEADER_VALUE_OFFSET;
            match key & KEY_KIND_MASK {
                KIND_INLINE_VALUE => memory.read_u32(value_address),
                KIND_INLINE_STRUCTURE => value_address,
                _ => header + memory.read_u32(value_address),
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: u32 = 0x1000;
    const SYSTEM_FLAGS: u32 = 0x0000_0400;
    const STRUCTURE_KEY: u32 = 0x0001_0201;
    const OFFSET_KEY: u32 = 0x0001_03FF;
    const STRUCTURE_OFFSET: u32 = 0x200;
    const MISSING_KEY: u32 = 0x0004_0006;

    fn header_with_fields() -> GuestMemory {
        let memory = GuestMemory::reserve().unwrap();
        let fields = [
            (SYSTEM_FLAGS_KEY, SYSTEM_FLAGS),
            (STRUCTURE_KEY, 0),
            (OFFSET_KEY, STRUCTURE_OFFSET),
        ];
        memory.write_u32(HEADER + OPTIONAL_HEADER_COUNT_OFFSET, fields.len() as u32);
        for (index, (key, value)) in (0..).zip(fields) {
            let entry = HEADER + OPTIONAL_HEADERS_OFFSET + index * OPTIONAL_HEADER_ENTRY_SIZE;
            memory.write_u32(entry, key);
            memory.write_u32(entry + OPTIONAL_HEADER_VALUE_OFFSET, value);
        }
        memory
    }

    #[test]
    fn header_bytes_span_the_recorded_size() {
        const HEADER_SIZE: u32 = 0x20;
        let mut file = vec![0; HEADER_SIZE as usize * 2];
        file[HEADER_SIZE_OFFSET..HEADER_SIZE_OFFSET + size_of::<u32>()]
            .copy_from_slice(&HEADER_SIZE.to_be_bytes());
        assert_eq!(
            header_bytes(&file).map(<[u8]>::len),
            Some(HEADER_SIZE as usize)
        );
        assert_eq!(header_bytes(&file[..HEADER_SIZE_OFFSET]), None);
    }

    #[test]
    fn each_key_kind_resolves_to_value_or_address() {
        let memory = header_with_fields();
        assert_eq!(field(&memory, HEADER, SYSTEM_FLAGS_KEY), Some(SYSTEM_FLAGS));
        let second_value = HEADER
            + OPTIONAL_HEADERS_OFFSET
            + OPTIONAL_HEADER_ENTRY_SIZE
            + OPTIONAL_HEADER_VALUE_OFFSET;
        assert_eq!(field(&memory, HEADER, STRUCTURE_KEY), Some(second_value));
        assert_eq!(
            field(&memory, HEADER, OFFSET_KEY),
            Some(HEADER + STRUCTURE_OFFSET)
        );
        assert_eq!(field(&memory, HEADER, MISSING_KEY), None);
    }
}
