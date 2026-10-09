//! File system imports. The title's disc is the directory holding the user's default.xex,
//! mounted read-only under the console's names for it.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use bevy::log::debug;

use super::{HANDLES, ImportCall, KernelObject, NtStatus, objects, read_ansi_string};
use crate::status;
use crate::time;
use crate::vfs::{self, DirectoryEntry, ResolveError, Vfs};

static VFS: RwLock<Vfs> = RwLock::new(Vfs::new());

/// The console's names for the game disc.
const DISC_DEVICES: [&str; 3] = ["game:", "d:", "\\Device\\Cdrom0"];
const PATH_SEPARATOR: char = '\\';
/// A pseudo-handle for the DOS devices directory (`\??\`): names relative to it are already
/// device paths such as `D:\`.
const DOS_DEVICES_ROOT: u32 = 0xFFFF_FFFD;

/// OBJECT_ATTRIBUTES: a root directory handle, then a pointer to an ANSI_STRING name.
const OBJECT_ATTRIBUTES_ROOT_DIRECTORY: u32 = 0x0;
const OBJECT_ATTRIBUTES_NAME: u32 = 0x4;
/// IO_STATUS_BLOCK: the status, then a request-specific value.
const IO_STATUS_STATUS: u32 = 0x0;
const IO_STATUS_INFORMATION: u32 = 0x4;

/// Access rights that would modify the file.
const WRITE_ACCESS: u32 = GENERIC_WRITE | FILE_WRITE_DATA | FILE_APPEND_DATA;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_WRITE_DATA: u32 = 0x2;
const FILE_APPEND_DATA: u32 = 0x4;
/// Create options.
const FILE_DIRECTORY_FILE: u32 = 0x1;
const FILE_NON_DIRECTORY_FILE: u32 = 0x40;

/// NtCreateFile's create disposition: what to do when the file does or does not exist.
#[derive(Clone, Copy, Debug)]
enum Disposition {
    Supersede = 0,
    Open = 1,
    Create = 2,
    OpenIf = 3,
    Overwrite = 4,
    OverwriteIf = 5,
}

impl Disposition {
    fn from_argument(value: u32) -> Option<Self> {
        [
            Self::Supersede,
            Self::Open,
            Self::Create,
            Self::OpenIf,
            Self::Overwrite,
            Self::OverwriteIf,
        ]
        .into_iter()
        .find(|disposition| *disposition as u32 == value)
    }
}

/// FILE_DIRECTORY_INFORMATION, the one entry NtQueryDirectoryFile returns per call.
const DIRECTORY_INFO_NEXT_ENTRY: u32 = 0x00;
const DIRECTORY_INFO_FILE_INDEX: u32 = 0x04;
const DIRECTORY_INFO_TIMES: [u32; 4] = [0x08, 0x10, 0x18, 0x20];
const DIRECTORY_INFO_END_OF_FILE: u32 = 0x28;
const DIRECTORY_INFO_ALLOCATION_SIZE: u32 = 0x30;
const DIRECTORY_INFO_ATTRIBUTES: u32 = 0x38;
const DIRECTORY_INFO_NAME_LENGTH: u32 = 0x3C;
const DIRECTORY_INFO_NAME: u32 = 0x40;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
/// The mask used when the title passes none.
const MATCH_EVERYTHING: &str = "*";
/// The only entry in a buffer has no next entry.
const LAST_ENTRY: u32 = 0;

/// NtQueryInformationFile information classes, and the size each needs.
const FILE_BASIC_INFORMATION: u32 = 4;
const FILE_STANDARD_INFORMATION: u32 = 5;
const FILE_POSITION_INFORMATION: u32 = 14;
const FILE_NETWORK_OPEN_INFORMATION: u32 = 34;
const FILE_ALLOCATION_INFORMATION: u32 = 19;
const FILE_END_OF_FILE_INFORMATION: u32 = 20;
/// FILE_POSITION_INFORMATION, FILE_ALLOCATION_INFORMATION and FILE_END_OF_FILE_INFORMATION are
/// each one 64-bit value.
const SIXTY_FOUR_BIT_INFORMATION_SIZE: u32 = 0x8;
const BASIC_INFORMATION_SIZE: u32 = 0x28;
const STANDARD_INFORMATION_SIZE: u32 = 0x18;
const POSITION_INFORMATION_SIZE: u32 = 0x8;
const NETWORK_OPEN_INFORMATION_SIZE: u32 = 0x38;
/// FILE_BASIC_INFORMATION: four times, then the attributes.
const BASIC_ATTRIBUTES: u32 = 0x20;
/// FILE_STANDARD_INFORMATION: allocation size, end of file, links, delete pending, directory.
const STANDARD_ALLOCATION_SIZE: u32 = 0x0;
const STANDARD_END_OF_FILE: u32 = 0x8;
const STANDARD_NUMBER_OF_LINKS: u32 = 0x10;
const STANDARD_DIRECTORY: u32 = 0x15;
/// FILE_NETWORK_OPEN_INFORMATION: four times, allocation size, end of file, attributes.
const NETWORK_OPEN_ALLOCATION_SIZE: u32 = 0x20;
const NETWORK_OPEN_END_OF_FILE: u32 = 0x28;
const NETWORK_OPEN_ATTRIBUTES: u32 = 0x30;
/// Every time field is 8 bytes; the four come first in each structure that has them.
const TIME_FIELD_SIZE: u32 = 8;
const TIME_FIELD_COUNT: u32 = 4;
/// Files on the disc have one directory entry each.
const SINGLE_LINK: u32 = 1;

/// NtReadFile's byte offset that means "at the current position".
const USE_CURRENT_POSITION: i64 = -2;

/// The IO_STATUS_BLOCK information value for a successful open: what the open did.
const FILE_SUPERSEDED: u32 = 0;
const FILE_OPENED: u32 = 1;
const FILE_CREATED: u32 = 2;
const FILE_OVERWRITTEN: u32 = 3;

/// An open file or directory.
pub(super) struct FileObject {
    /// The full guest path, used to resolve paths relative to a directory handle.
    guest_path: String,
    host_path: PathBuf,
    state: Mutex<FileState>,
}

struct FileState {
    /// None for directories.
    file: Option<File>,
    position: u64,
    /// A directory enumeration in progress: the matching entries, and how many were returned.
    listing: Option<(Vec<DirectoryEntry>, usize)>,
}

/// Mounts the disc read-only; called once before the guest starts.
pub fn mount_disc(root: &Path) {
    let mut vfs = VFS.write().unwrap();
    for device in DISC_DEVICES {
        vfs.mount(device, root.to_path_buf(), false);
    }
}

/// Mounts a writable directory, such as an open save package, at `device`.
pub(super) fn mount_writable(device: &str, root: PathBuf) {
    VFS.write().unwrap().mount(device, root, true);
}

/// Returns whether anything was mounted at `device`.
pub(super) fn unmount(device: &str) -> bool {
    VFS.write().unwrap().unmount(device)
}

/// NtOpenFile(handle out, access, attributes, I/O status out, share access, open options).
pub(super) fn nt_open_file(call: &ImportCall) -> u32 {
    open(call, Disposition::Open as u32, call.argument(5))
}

/// NtCreateFile(handle out, access, attributes, I/O status out, allocation size, attributes,
/// share access, disposition, create options).
pub(super) fn nt_create_file(call: &ImportCall) -> u32 {
    open(call, call.argument(7), call.argument(8))
}

fn open(call: &ImportCall, disposition: u32, options: u32) -> u32 {
    let (handle_out, access, attributes, io_status) = (
        call.argument(0),
        call.argument(1),
        call.argument(2),
        call.argument(3),
    );
    let Some(disposition) = Disposition::from_argument(disposition) else {
        return NtStatus::INVALID_PARAMETER.0;
    };
    let Some(guest_path) = object_path(call, attributes) else {
        return NtStatus::INVALID_HANDLE.0;
    };

    let result = open_path(&guest_path, access, disposition, options);
    debug!(target: "file", "open {guest_path:?} ({disposition:?}, access {access:#x}) -> {:#x}",
        result.as_ref().err().map_or(0, |status| status.0));
    let information = result.as_ref().map_or(0, |(_, information)| *information);
    let status = match result.map(|(object, _)| object) {
        Ok(object) => match HANDLES
            .lock()
            .unwrap()
            .insert(KernelObject::File(Arc::new(object)))
        {
            Some(handle) => {
                call.memory.write_u32(handle_out, handle);
                NtStatus::SUCCESS
            }
            None => NtStatus::INSUFFICIENT_RESOURCES,
        },
        Err(status) => status,
    };
    call.memory
        .write_u32(io_status + IO_STATUS_STATUS, status.0);
    call.memory
        .write_u32(io_status + IO_STATUS_INFORMATION, information);
    status.0
}

/// Opens or creates a file or directory, returning it with the IO_STATUS_BLOCK information
/// value that says which happened. Only writable mounts (save packages) create, overwrite or
/// grant write access; on the disc only opening an existing file can succeed.
fn open_path(
    guest_path: &str,
    access: u32,
    disposition: Disposition,
    options: u32,
) -> Result<(FileObject, u32), NtStatus> {
    let vfs = VFS.read().unwrap();
    let writable = vfs.is_writable(guest_path);
    let wants_directory = options & FILE_DIRECTORY_FILE != 0;
    let (host_path, information) = match vfs.resolve(guest_path) {
        Ok(path) => {
            let information = match disposition {
                Disposition::Open | Disposition::OpenIf => FILE_OPENED,
                Disposition::Create => return Err(NtStatus::OBJECT_NAME_COLLISION),
                _ if !writable => return Err(NtStatus::MEDIA_WRITE_PROTECTED),
                Disposition::Supersede => FILE_SUPERSEDED,
                Disposition::Overwrite | Disposition::OverwriteIf => FILE_OVERWRITTEN,
            };
            if information != FILE_OPENED && path.is_file() {
                File::create(&path).map_err(|_| NtStatus::ACCESS_DENIED)?;
            }
            (path, information)
        }
        Err(ResolveError::UnknownDevice) => {
            debug!(target: "file", "no mount for {guest_path:?}");
            return Err(NtStatus::OBJECT_PATH_NOT_FOUND);
        }
        Err(ResolveError::PathNotFound) => return Err(NtStatus::OBJECT_PATH_NOT_FOUND),
        Err(ResolveError::NameNotFound { parent, name }) => {
            match disposition {
                Disposition::Open | Disposition::Overwrite => {
                    return Err(NtStatus::OBJECT_NAME_NOT_FOUND);
                }
                _ if !writable => return Err(NtStatus::MEDIA_WRITE_PROTECTED),
                _ => {}
            }
            let path = parent.join(name);
            let created = if wants_directory {
                fs::create_dir(&path)
            } else {
                File::create_new(&path).map(drop)
            };
            created.map_err(|_| NtStatus::ACCESS_DENIED)?;
            (path, FILE_CREATED)
        }
    };
    drop(vfs);

    let wants_write = access & WRITE_ACCESS != 0;
    if wants_write && !writable {
        return Err(NtStatus::MEDIA_WRITE_PROTECTED);
    }
    let is_directory = host_path.is_dir();
    if wants_directory && !is_directory {
        return Err(NtStatus::NOT_A_DIRECTORY);
    }
    if options & FILE_NON_DIRECTORY_FILE != 0 && is_directory {
        return Err(NtStatus::FILE_IS_A_DIRECTORY);
    }

    let file = if is_directory {
        None
    } else {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(writable)
            .open(&host_path)
            .map_err(|_| NtStatus::ACCESS_DENIED)?;
        Some(file)
    };
    let object = FileObject {
        guest_path: guest_path.to_string(),
        host_path,
        state: Mutex::new(FileState {
            file,
            position: 0,
            listing: None,
        }),
    };
    Ok((object, information))
}

/// The full guest path an OBJECT_ATTRIBUTES names, joined to its root directory's path if it
/// has one. None if the root directory handle is not an open directory.
fn object_path(call: &ImportCall, attributes: u32) -> Option<String> {
    let name = read_ansi_string(
        call,
        call.memory.read_u32(attributes + OBJECT_ATTRIBUTES_NAME),
    );

    let root = call
        .memory
        .read_u32(attributes + OBJECT_ATTRIBUTES_ROOT_DIRECTORY);
    if root == 0 || root == DOS_DEVICES_ROOT {
        return Some(name);
    }
    match HANDLES.lock().unwrap().get(root) {
        Some(KernelObject::File(directory)) => Some(format!(
            "{}{PATH_SEPARATOR}{name}",
            directory.guest_path.trim_end_matches(PATH_SEPARATOR)
        )),
        _ => {
            debug!(target: "file", "{name:?} is relative to {root:#x}, which is not an open directory");
            None
        }
    }
}

/// What NtReadFile and NtWriteFile share: (handle, event, APC routine, APC context, I/O status
/// out, buffer, length, byte offset).
struct Transfer {
    object: Arc<FileObject>,
    event: u32,
    io_status: u32,
    buffer: u32,
    length: u32,
    /// None means the current position.
    offset: Option<i64>,
}

impl Transfer {
    fn parse(call: &ImportCall, name: &str) -> Result<Self, NtStatus> {
        let apc_routine = call.argument(2);
        if apc_routine != 0 {
            status::halt_guest_thread(format!(
                "{name} with an APC routine ({apc_routine:#010x}) is not supported yet"
            ));
        }
        let Some(KernelObject::File(object)) = HANDLES.lock().unwrap().get(call.argument(0)) else {
            return Err(NtStatus::INVALID_HANDLE);
        };
        let offset_pointer = call.argument(7);
        Ok(Self {
            object,
            event: call.argument(1),
            io_status: call.argument(4),
            buffer: call.argument(5),
            length: call.argument(6),
            offset: (offset_pointer != 0)
                .then(|| call.memory.read_i64(offset_pointer))
                .filter(|&offset| offset != USE_CURRENT_POSITION),
        })
    }

    /// Reports the outcome; transfers complete synchronously, then signal the event if any.
    fn finish(&self, call: &ImportCall, status: NtStatus, transferred: u32) -> u32 {
        call.memory
            .write_u32(self.io_status + IO_STATUS_STATUS, status.0);
        call.memory
            .write_u32(self.io_status + IO_STATUS_INFORMATION, transferred);
        if self.event != 0 {
            objects::signal_event(call, self.event);
        }
        status.0
    }
}

/// NtReadFile(handle, event, APC routine, APC context, I/O status out, buffer, length,
/// byte offset).
pub(super) fn nt_read_file(call: &ImportCall) -> u32 {
    let transfer = match Transfer::parse(call, "NtReadFile") {
        Ok(transfer) => transfer,
        Err(status) => return status.0,
    };
    let (status, transferred) = match read(&transfer.object, transfer.offset, transfer.length) {
        Ok(bytes) => {
            call.memory.write_bytes(transfer.buffer, &bytes);
            let status = if bytes.is_empty() && transfer.length > 0 {
                NtStatus::END_OF_FILE
            } else {
                NtStatus::SUCCESS
            };
            (status, bytes.len() as u32)
        }
        Err(status) => (status, 0),
    };
    debug!(target: "file", "read {:?} at {:?}: {transferred} of {} bytes -> {:#x}",
        transfer.object.guest_path, transfer.offset, transfer.length, status.0);
    transfer.finish(call, status, transferred)
}

/// NtWriteFile(handle, event, APC routine, APC context, I/O status out, buffer, length,
/// byte offset).
pub(super) fn nt_write_file(call: &ImportCall) -> u32 {
    let transfer = match Transfer::parse(call, "NtWriteFile") {
        Ok(transfer) => transfer,
        Err(status) => return status.0,
    };
    let bytes: Vec<u8> = (0..transfer.length)
        .map(|offset| call.memory.read_u8(transfer.buffer + offset))
        .collect();
    let status = match write(&transfer.object, transfer.offset, &bytes) {
        Ok(()) => NtStatus::SUCCESS,
        Err(status) => status,
    };
    let transferred = if status.0 == NtStatus::SUCCESS.0 {
        transfer.length
    } else {
        0
    };
    debug!(target: "file", "write {:?} at {:?}: {transferred} bytes -> {:#x}",
        transfer.object.guest_path, transfer.offset, status.0);
    transfer.finish(call, status, transferred)
}

/// Writes `bytes` at `offset` (or the current position), advancing the position.
fn write(object: &FileObject, offset: Option<i64>, bytes: &[u8]) -> Result<(), NtStatus> {
    let mut state = object.state.lock().unwrap();
    let start = match offset {
        Some(offset) => u64::try_from(offset).map_err(|_| NtStatus::INVALID_PARAMETER)?,
        None => state.position,
    };
    let file = state
        .file
        .as_mut()
        .ok_or(NtStatus::INVALID_DEVICE_REQUEST)?;
    file.seek(SeekFrom::Start(start))
        .and_then(|_| file.write_all(bytes))
        .map_err(|_| NtStatus::ACCESS_DENIED)?;
    state.position = start + bytes.len() as u64;
    Ok(())
}

/// NtFlushBuffersFile(handle, I/O status out) -> NTSTATUS. Commits written data to the host disk.
pub(super) fn nt_flush_buffers_file(call: &ImportCall) -> u32 {
    let Some(KernelObject::File(object)) = HANDLES.lock().unwrap().get(call.argument(0)) else {
        return NtStatus::INVALID_HANDLE.0;
    };
    let status = match object.state.lock().unwrap().file.as_mut() {
        Some(file) => match file.sync_data() {
            Ok(()) => NtStatus::SUCCESS,
            Err(_) => NtStatus::UNEXPECTED_IO_ERROR,
        },
        None => NtStatus::SUCCESS,
    };
    let io_status = call.argument(1);
    call.memory
        .write_u32(io_status + IO_STATUS_STATUS, status.0);
    call.memory.write_u32(io_status + IO_STATUS_INFORMATION, 0);
    status.0
}

/// Reads up to `length` bytes at `offset` (or the current position), advancing the position.
fn read(object: &FileObject, offset: Option<i64>, length: u32) -> Result<Vec<u8>, NtStatus> {
    let mut state = object.state.lock().unwrap();
    let start = match offset {
        Some(offset) => u64::try_from(offset).map_err(|_| NtStatus::INVALID_PARAMETER)?,
        None => state.position,
    };
    let file = state
        .file
        .as_mut()
        .ok_or(NtStatus::INVALID_DEVICE_REQUEST)?;
    file.seek(SeekFrom::Start(start))
        .map_err(|_| NtStatus::INVALID_PARAMETER)?;
    let mut bytes = Vec::new();
    file.take(u64::from(length))
        .read_to_end(&mut bytes)
        .map_err(|_| NtStatus::UNEXPECTED_IO_ERROR)?;
    state.position = start + bytes.len() as u64;
    Ok(bytes)
}

/// NtQueryDirectoryFile(handle, event, APC routine, APC context, I/O status out, buffer,
/// length, file mask, restart scan). Returns one matching entry per call; the first call (or a
/// restart) takes the mask and snapshots the directory.
pub(super) fn nt_query_directory_file(call: &ImportCall) -> u32 {
    let (handle, event, io_status) = (call.argument(0), call.argument(1), call.argument(4));
    let (buffer, length, mask, restart) = (
        call.argument(5),
        call.argument(6),
        call.argument(7),
        call.argument(8) != 0,
    );
    let Some(KernelObject::File(object)) = HANDLES.lock().unwrap().get(handle) else {
        return NtStatus::INVALID_HANDLE.0;
    };
    let mut state = object.state.lock().unwrap();
    if state.file.is_some() {
        return NtStatus::INVALID_PARAMETER.0;
    }

    let first_call = restart || state.listing.is_none();
    if first_call {
        let mask = if mask == 0 {
            MATCH_EVERYTHING.to_string()
        } else {
            read_ansi_string(call, mask)
        };
        let Ok(entries) = vfs::list_directory(&object.host_path) else {
            return NtStatus::UNEXPECTED_IO_ERROR.0;
        };
        let matching = entries
            .into_iter()
            .filter(|entry| vfs::matches_mask(&entry.name, &mask))
            .collect();
        state.listing = Some((matching, 0));
    }
    let (entries, returned) = state.listing.as_mut().unwrap();

    let (status, written) = match entries.get(*returned) {
        None if first_call => (NtStatus::NO_SUCH_FILE, 0),
        None => (NtStatus::NO_MORE_FILES, 0),
        Some(entry) => {
            let required = DIRECTORY_INFO_NAME + entry.name.len() as u32;
            if length < required {
                (NtStatus::BUFFER_OVERFLOW, 0)
            } else {
                write_directory_entry(call, buffer, *returned as u32, entry);
                *returned += 1;
                (NtStatus::SUCCESS, required)
            }
        }
    };
    drop(state);
    call.memory
        .write_u32(io_status + IO_STATUS_STATUS, status.0);
    call.memory
        .write_u32(io_status + IO_STATUS_INFORMATION, written);
    if event != 0 {
        objects::signal_event(call, event);
    }
    status.0
}

fn write_directory_entry(call: &ImportCall, buffer: u32, index: u32, entry: &DirectoryEntry) {
    let memory = call.memory;
    let modified = time::filetime(entry.modified);
    memory.write_u32(buffer + DIRECTORY_INFO_NEXT_ENTRY, LAST_ENTRY);
    memory.write_u32(buffer + DIRECTORY_INFO_FILE_INDEX, index);
    for offset in DIRECTORY_INFO_TIMES {
        memory.write_u64(buffer + offset, modified);
    }
    memory.write_u64(buffer + DIRECTORY_INFO_END_OF_FILE, entry.size);
    memory.write_u64(buffer + DIRECTORY_INFO_ALLOCATION_SIZE, entry.size);
    let attributes = if entry.is_directory {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    memory.write_u32(buffer + DIRECTORY_INFO_ATTRIBUTES, attributes);
    memory.write_u32(buffer + DIRECTORY_INFO_NAME_LENGTH, entry.name.len() as u32);
    memory.write_bytes(buffer + DIRECTORY_INFO_NAME, entry.name.as_bytes());
}

/// NtQueryInformationFile(handle, I/O status out, buffer, length, information class).
pub(super) fn nt_query_information_file(call: &ImportCall) -> u32 {
    let (handle, io_status, buffer, length, class) = (
        call.argument(0),
        call.argument(1),
        call.argument(2),
        call.argument(3),
        call.argument(4),
    );
    let Some(KernelObject::File(object)) = HANDLES.lock().unwrap().get(handle) else {
        return NtStatus::INVALID_HANDLE.0;
    };
    let required = match class {
        FILE_BASIC_INFORMATION => BASIC_INFORMATION_SIZE,
        FILE_STANDARD_INFORMATION => STANDARD_INFORMATION_SIZE,
        FILE_POSITION_INFORMATION => POSITION_INFORMATION_SIZE,
        FILE_NETWORK_OPEN_INFORMATION => NETWORK_OPEN_INFORMATION_SIZE,
        _ => status::halt_guest_thread(format!(
            "NtQueryInformationFile class {class} on {:?} is not supported yet",
            object.guest_path
        )),
    };
    let status = if length < required {
        NtStatus::INFO_LENGTH_MISMATCH
    } else {
        match std::fs::metadata(&object.host_path) {
            Ok(metadata) => {
                write_file_information(call, &object, &metadata, class, buffer);
                NtStatus::SUCCESS
            }
            Err(_) => NtStatus::UNEXPECTED_IO_ERROR,
        }
    };
    let written = if status.0 == NtStatus::SUCCESS.0 {
        required
    } else {
        0
    };
    call.memory
        .write_u32(io_status + IO_STATUS_STATUS, status.0);
    call.memory
        .write_u32(io_status + IO_STATUS_INFORMATION, written);
    status.0
}

/// NtSetInformationFile(handle, I/O status out, buffer, length, class) -> NTSTATUS.
pub(super) fn nt_set_information_file(call: &ImportCall) -> u32 {
    let (handle, io_status, buffer, length, class) = (
        call.argument(0),
        call.argument(1),
        call.argument(2),
        call.argument(3),
        call.argument(4),
    );
    let Some(KernelObject::File(object)) = HANDLES.lock().unwrap().get(handle) else {
        return NtStatus::INVALID_HANDLE.0;
    };
    let required = match class {
        FILE_BASIC_INFORMATION => BASIC_INFORMATION_SIZE,
        FILE_POSITION_INFORMATION | FILE_ALLOCATION_INFORMATION | FILE_END_OF_FILE_INFORMATION => {
            SIXTY_FOUR_BIT_INFORMATION_SIZE
        }
        _ => status::halt_guest_thread(format!(
            "NtSetInformationFile class {class} on {:?} is not supported yet",
            object.guest_path
        )),
    };
    let status = if length < required {
        NtStatus::INFO_LENGTH_MISMATCH
    } else if class == FILE_BASIC_INFORMATION {
        // Times and attributes are not kept: the host file's own stand in for them.
        NtStatus::SUCCESS
    } else {
        let value = call.memory.read_i64(buffer) as u64;
        match object.state.lock().unwrap().set_information(class, value) {
            Ok(()) => NtStatus::SUCCESS,
            Err(status) => status,
        }
    };
    debug!(target: "file", "set information {class} on {:?} -> {:#x}", object.guest_path, status.0);
    call.memory
        .write_u32(io_status + IO_STATUS_STATUS, status.0);
    call.memory.write_u32(io_status + IO_STATUS_INFORMATION, 0);
    status.0
}

impl FileState {
    /// Applies one of the 64-bit information classes.
    fn set_information(&mut self, class: u32, value: u64) -> Result<(), NtStatus> {
        if class == FILE_POSITION_INFORMATION {
            self.position = value;
            return Ok(());
        }
        let file = self.file.as_mut().ok_or(NtStatus::INVALID_DEVICE_REQUEST)?;
        let length = file
            .metadata()
            .map_err(|_| NtStatus::UNEXPECTED_IO_ERROR)?
            .len();
        // Reserving space changes nothing visible, but an allocation smaller than the file
        // cuts it short, as on NT.
        let new_length = match class {
            FILE_END_OF_FILE_INFORMATION => value,
            _ => length.min(value),
        };
        file.set_len(new_length)
            .map_err(|_| NtStatus::ACCESS_DENIED)
    }
}

fn write_file_information(
    call: &ImportCall,
    object: &FileObject,
    metadata: &std::fs::Metadata,
    class: u32,
    buffer: u32,
) {
    let memory = call.memory;
    let size = if metadata.is_dir() { 0 } else { metadata.len() };
    let attributes = if metadata.is_dir() {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    let write_times = || {
        let modified = time::filetime(metadata.modified().unwrap_or(std::time::UNIX_EPOCH));
        for index in 0..TIME_FIELD_COUNT {
            memory.write_u64(buffer + index * TIME_FIELD_SIZE, modified);
        }
    };
    match class {
        FILE_BASIC_INFORMATION => {
            memory.write_bytes(buffer, &[0; BASIC_INFORMATION_SIZE as usize]);
            write_times();
            memory.write_u32(buffer + BASIC_ATTRIBUTES, attributes);
        }
        FILE_STANDARD_INFORMATION => {
            memory.write_bytes(buffer, &[0; STANDARD_INFORMATION_SIZE as usize]);
            memory.write_u64(buffer + STANDARD_ALLOCATION_SIZE, size);
            memory.write_u64(buffer + STANDARD_END_OF_FILE, size);
            memory.write_u32(buffer + STANDARD_NUMBER_OF_LINKS, SINGLE_LINK);
            memory.write_u8(buffer + STANDARD_DIRECTORY, u8::from(metadata.is_dir()));
        }
        FILE_POSITION_INFORMATION => {
            memory.write_u64(buffer, object.state.lock().unwrap().position);
        }
        FILE_NETWORK_OPEN_INFORMATION => write_network_open_information(call, metadata, buffer),
        _ => unreachable!("the caller checked the class"),
    }
}

fn write_network_open_information(call: &ImportCall, metadata: &std::fs::Metadata, buffer: u32) {
    let memory = call.memory;
    let size = if metadata.is_dir() { 0 } else { metadata.len() };
    let attributes = if metadata.is_dir() {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    let modified = time::filetime(metadata.modified().unwrap_or(std::time::UNIX_EPOCH));
    memory.write_bytes(buffer, &[0; NETWORK_OPEN_INFORMATION_SIZE as usize]);
    for index in 0..TIME_FIELD_COUNT {
        memory.write_u64(buffer + index * TIME_FIELD_SIZE, modified);
    }
    memory.write_u64(buffer + NETWORK_OPEN_ALLOCATION_SIZE, size);
    memory.write_u64(buffer + NETWORK_OPEN_END_OF_FILE, size);
    memory.write_u32(buffer + NETWORK_OPEN_ATTRIBUTES, attributes);
}

/// NtQueryFullAttributesFile(attributes, information out) -> NTSTATUS. The information is a
/// FILE_NETWORK_OPEN_INFORMATION for the path the attributes name.
pub(super) fn nt_query_full_attributes_file(call: &ImportCall) -> u32 {
    let (attributes, buffer) = (call.argument(0), call.argument(1));
    let Some(guest_path) = object_path(call, attributes) else {
        return NtStatus::INVALID_HANDLE.0;
    };
    let resolved = VFS.read().unwrap().resolve(&guest_path);
    let status = match resolved {
        Ok(host_path) => match std::fs::metadata(host_path) {
            Ok(metadata) => {
                write_network_open_information(call, &metadata, buffer);
                NtStatus::SUCCESS
            }
            Err(_) => NtStatus::UNEXPECTED_IO_ERROR,
        },
        Err(ResolveError::NameNotFound { .. }) => NtStatus::OBJECT_NAME_NOT_FOUND,
        Err(ResolveError::PathNotFound | ResolveError::UnknownDevice) => {
            NtStatus::OBJECT_PATH_NOT_FOUND
        }
    };
    debug!(target: "file", "attributes of {guest_path:?} -> {:#x}", status.0);
    status.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const WRITTEN: &[u8] = b"0123456789";
    const SHORTER: u64 = 4;
    const LONGER: u64 = 16;

    fn state_with_file(name: &str) -> (FileState, PathBuf) {
        let path = std::env::temp_dir().join(format!("pure-runtime-{name}-{}", std::process::id()));
        fs::write(&path, WRITTEN).unwrap();
        let file = File::options().read(true).write(true).open(&path).unwrap();
        let state = FileState {
            file: Some(file),
            position: 0,
            listing: None,
        };
        (state, path)
    }

    #[test]
    fn allocation_only_ever_cuts_the_file_short() {
        let (mut state, path) = state_with_file("allocation");
        state
            .set_information(FILE_ALLOCATION_INFORMATION, LONGER)
            .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), WRITTEN.len() as u64);
        state
            .set_information(FILE_ALLOCATION_INFORMATION, SHORTER)
            .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), SHORTER);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn end_of_file_sets_the_length_either_way() {
        let (mut state, path) = state_with_file("end-of-file");
        state
            .set_information(FILE_END_OF_FILE_INFORMATION, LONGER)
            .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), LONGER);
        state
            .set_information(FILE_END_OF_FILE_INFORMATION, SHORTER)
            .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), SHORTER);
        fs::remove_file(path).unwrap();
    }
}
