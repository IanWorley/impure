//! The game library: the code XenonRecomp generated from the player's own `default.xex`, with
//! the glue and title fixes in cpp/, compiled on the player's machine by the builder
//! (src/builder.rs) and loaded here at run time. The runtime itself never contains game code, so
//! every build of it can be published.
//!
//! The library calls back into the runtime only through the `Host` table handed over at load
//! (cpp/host.h), and exports the functions below.

use std::ffi::c_char;
use std::path::Path;
use std::sync::OnceLock;

use libloading::Library;

use crate::kernel::{self, GuestContext};

/// Must match kPureHostAbiVersion in cpp/host.h.
const HOST_ABI_VERSION: u32 = 1;

/// The runtime's callbacks, in cpp/host.h's PureHost layout.
#[repr(C)]
struct Host {
    import_call: extern "C" fn(*const c_char, *mut GuestContext, *mut u8),
    resolve_variable_import: extern "C" fn(*const c_char) -> u32,
    unmapped_call_halt: extern "C" fn(u32, u32) -> !,
}

static HOST: Host = Host {
    import_call: kernel::import_call,
    resolve_variable_import: kernel::resolve_variable_import,
    unmapped_call_halt: kernel::unmapped_call_halt,
};

/// Where the guest-address -> host-function lookup table lies, for the crash reporter.
#[derive(Clone, Copy)]
#[cfg_attr(
    not(unix),
    expect(dead_code, reason = "read by the crash reporter, which is Unix-only")
)]
pub struct LookupTable {
    /// From the guest memory base.
    pub offset: u64,
    pub code_base: u32,
    pub code_size: u32,
}

pub struct GameModule {
    /// Kept loaded for the life of the process: the function pointers below point into it.
    _library: Library,
    pub guest_load: unsafe extern "C" fn(*const u8, usize, *mut u8) -> u32,
    pub guest_run: unsafe extern "C" fn(*mut u8, u32, u32, u32, u32, u32) -> u32,
    pub title_fix_count: unsafe extern "C" fn() -> u32,
    pub ctx_argument: unsafe extern "C" fn(*const GuestContext, u32) -> u64,
    pub ctx_set_return: unsafe extern "C" fn(*mut GuestContext, u64),
    pub ctx_return_address: unsafe extern "C" fn(*const GuestContext) -> u32,
    pub ctx_stack_pointer: unsafe extern "C" fn(*const GuestContext) -> u32,
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "read by the crash reporter, which is Unix-only")
    )]
    pub lookup_table: LookupTable,
}

static MODULE: OnceLock<GameModule> = OnceLock::new();

/// Loads the library at `path` and hands it the runtime's callbacks. Only one can be loaded.
pub fn load(path: &Path) -> Result<&'static GameModule, String> {
    let describe = |error: libloading::Error| format!("{}: {error}", path.display());
    // SAFETY: the library is one the builder compiled from cpp/ and the generated code; its
    // initializers only set up C++ statics.
    let library = unsafe { Library::new(path) }.map_err(describe)?;
    // SAFETY: each symbol is looked up with the type cpp/glue.cpp and cpp/patches.cpp define it
    // with; the copies stay valid because the library is kept loaded in the module.
    let module = unsafe {
        let attach = *library
            .get::<unsafe extern "C" fn(*const Host) -> u32>(b"pure_attach_host")
            .map_err(describe)?;
        let version = attach(&HOST);
        if version != HOST_ABI_VERSION {
            return Err(format!(
                "{} was built for runtime interface {version}, this runtime needs {HOST_ABI_VERSION}",
                path.display()
            ));
        }
        let data = |name: &[u8]| library.get::<*const u8>(name).map(|symbol| *symbol);
        let lookup_table = LookupTable {
            offset: *data(b"pure_lookup_table_offset")
                .map_err(describe)?
                .cast::<u64>(),
            code_base: *data(b"pure_code_base").map_err(describe)?.cast::<u32>(),
            code_size: *data(b"pure_code_size").map_err(describe)?.cast::<u32>(),
        };
        GameModule {
            guest_load: *library.get(b"pure_guest_load").map_err(describe)?,
            guest_run: *library.get(b"pure_guest_run").map_err(describe)?,
            title_fix_count: *library.get(b"pure_title_fix_count").map_err(describe)?,
            ctx_argument: *library.get(b"pure_ctx_argument").map_err(describe)?,
            ctx_set_return: *library.get(b"pure_ctx_set_return").map_err(describe)?,
            ctx_return_address: *library.get(b"pure_ctx_return_address").map_err(describe)?,
            ctx_stack_pointer: *library.get(b"pure_ctx_stack_pointer").map_err(describe)?,
            lookup_table,
            _library: library,
        }
    };
    MODULE
        .set(module)
        .map_err(|_| "a game library is already loaded".to_string())?;
    Ok(get())
}

/// The loaded library. Panics if none is: guest code only runs after `load`.
pub fn get() -> &'static GameModule {
    MODULE.get().expect("the game library is not loaded")
}

/// The loaded library, if any; for the crash reporter, which may run before one is.
#[cfg_attr(
    not(unix),
    expect(dead_code, reason = "read by the crash reporter, which is Unix-only")
)]
pub fn try_get() -> Option<&'static GameModule> {
    MODULE.get()
}
