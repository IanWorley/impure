//! Xbox 360 kernel (xboxkrnl.exe) and Xam imports.
//!
//! The generated code calls every import through `pure_import_call` with the import's name.
//! None are implemented yet: each call is logged and stops the calling guest thread.

use std::ffi::{CStr, c_char};

use bevy::log::info;

use crate::guest;
use crate::status;

/// Size of the zero-filled block backing each variable import (exported kernel data).
const VARIABLE_IMPORT_SIZE: u32 = 0x1000;
const VARIABLE_IMPORT_ALIGNMENT: u32 = 0x10;
/// How many argument registers to show when logging an import call.
const LOGGED_ARGUMENT_COUNT: u32 = 4;

/// Opaque `PPCContext` from the generated code.
#[repr(C)]
pub struct GuestContext {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn pure_ctx_argument(ctx: *const GuestContext, index: u32) -> u64;
    fn pure_ctx_return_address(ctx: *const GuestContext) -> u32;
}

/// Called by the generated import forwarders on the guest thread.
#[unsafe(no_mangle)]
extern "C" fn pure_import_call(name: *const c_char, ctx: *mut GuestContext, _base: *mut u8) {
    // SAFETY: the forwarders pass string literals and the live guest context.
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    let arguments: Vec<String> = (0..LOGGED_ARGUMENT_COUNT)
        // SAFETY: ctx is the caller's live context.
        .map(|index| {
            format!("r{}={:#x}", index + 3, unsafe {
                pure_ctx_argument(ctx, index)
            })
        })
        .collect();
    // SAFETY: as above.
    let return_address = unsafe { pure_ctx_return_address(ctx) };

    info!(target: "import", "{name}({}) from {return_address:#010x}", arguments.join(", "));
    status::halt_guest_thread(format!(
        "unimplemented import {name} called from {return_address:#010x}"
    ));
}

/// Called on the guest thread when an indirect call targets an address with no recompiled
/// function.
#[unsafe(no_mangle)]
extern "C" fn pure_unmapped_call_halt(target: u32, return_address: u32) -> ! {
    status::halt_guest_thread(format!(
        "indirect call to {target:#010x}, which has no recompiled function (return address {return_address:#010x})"
    ));
}

/// Called by the loader for each variable import; returns the guest address of its data.
#[unsafe(no_mangle)]
extern "C" fn pure_resolve_variable_import(name: *const c_char) -> u32 {
    // SAFETY: the loader passes a NUL-terminated name that outlives this call.
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    let address = guest::allocate(VARIABLE_IMPORT_SIZE, VARIABLE_IMPORT_ALIGNMENT)
        .expect("system heap exhausted while resolving variable imports");
    info!(target: "import", "variable {name} -> {address:#010x} (zero-filled)");
    address
}
