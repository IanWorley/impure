//! Executable loader imports, answered from the XEX header the loader copied into guest memory.

use super::{FALSE, ImportCall, TRUE};
use crate::{guest, status, xex_header};

/// Privileges are bit numbers in the 32-bit system flags field.
const SYSTEM_FLAG_BITS: u32 = u32::BITS;
/// RtlImageXexHeaderField's result for a field the header does not have.
const MISSING_FIELD: u32 = 0;

/// XexCheckExecutablePrivilege(privilege) -> BOOLEAN.
pub(super) fn check_executable_privilege(call: &ImportCall) -> u32 {
    let privilege = call.argument(0);
    if privilege >= SYSTEM_FLAG_BITS {
        status::halt_guest_thread(format!(
            "XexCheckExecutablePrivilege({privilege}) needs the extended system flags, which the runtime does not read"
        ));
    }
    let flags = xex_header::field(
        call.memory,
        guest::xex_header(),
        xex_header::SYSTEM_FLAGS_KEY,
    )
    .unwrap_or(0);
    if flags & (1 << privilege) != 0 {
        TRUE
    } else {
        FALSE
    }
}

/// RtlImageXexHeaderField(header, key) -> the field's value or address, or 0 if absent.
pub(super) fn image_xex_header_field(call: &ImportCall) -> u32 {
    xex_header::field(call.memory, call.argument(0), call.argument(1)).unwrap_or(MISSING_FIELD)
}
