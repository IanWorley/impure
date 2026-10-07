//! Xbox 360 kernel (xboxkrnl.exe) and Xam imports.
//!
//! The generated code calls every import through `pure_import_call` with the import's name.
//! Implemented imports run their handler and return to the guest; any other import is logged and
//! stops the calling guest thread, so the stop line names the next import to implement.

mod audio;
mod content;
mod files;
mod input;
mod irql;
mod memory;
mod network;
mod notify;
mod objects;
mod overlapped;
mod rtl;
mod system;
mod threads;
mod timers;
mod tls;
mod user;
mod video;
mod xex;
mod xma;
mod xmsg;

use std::ffi::{CStr, c_char};
use std::sync::{Arc, Mutex};

use bevy::log::{debug, info};

use crate::guest;
use crate::handles::HandleTable;
use crate::memory::GuestMemory;
use crate::{status, timebase};

/// Size of the zero-filled block backing each variable import (exported kernel data).
const VARIABLE_IMPORT_SIZE: u32 = 0x1000;
const VARIABLE_IMPORT_ALIGNMENT: u32 = 0x10;
/// Arguments 0-7 arrive in r3-r10; later ones are 32-bit words in 8-byte stack slots that start
/// at this offset from the caller's stack pointer.
const REGISTER_ARGUMENT_COUNT: u32 = 8;
const STACK_ARGUMENTS_OFFSET: u32 = 0x54;
const STACK_ARGUMENT_SLOT_SIZE: u32 = 8;
/// How many argument registers to show when logging an import call.
const LOGGED_ARGUMENT_COUNT: u32 = 4;

/// What a handle names.
#[derive(Clone)]
enum KernelObject {
    /// A waitable object, by the guest address of its DISPATCHER_HEADER.
    Dispatcher(u32),
    File(Arc<files::FileObject>),
    /// A system notification listener.
    Listener(Arc<notify::Listener>),
    /// An enumeration of content packages.
    Enumerator(Arc<Mutex<content::Enumerator>>),
}

pub use files::mount_disc;
pub use threads::start_main_thread;

static HANDLES: Mutex<HandleTable<KernelObject>> = Mutex::new(HandleTable::new());

/// Opaque `PPCContext` from the generated code.
#[repr(C)]
pub struct GuestContext {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn pure_ctx_argument(ctx: *const GuestContext, index: u32) -> u64;
    fn pure_ctx_set_return(ctx: *mut GuestContext, value: u64);
    fn pure_ctx_return_address(ctx: *const GuestContext) -> u32;
    fn pure_ctx_stack_pointer(ctx: *const GuestContext) -> u32;
}

/// NTSTATUS values the handlers return.
#[derive(Clone, Copy)]
pub(super) struct NtStatus(pub(super) u32);

impl NtStatus {
    pub(super) const SUCCESS: Self = Self(0);
    /// Informational: a create found an existing object with the name and opened it.
    pub(super) const OBJECT_NAME_EXISTS: Self = Self(0x4000_0000);
    pub(super) const NO_MORE_FILES: Self = Self(0x8000_0006);
    pub(super) const BUFFER_OVERFLOW: Self = Self(0x8000_0005);
    pub(super) const NO_SUCH_FILE: Self = Self(0xC000_000F);
    pub(super) const END_OF_FILE: Self = Self(0xC000_0011);
    pub(super) const INVALID_DEVICE_REQUEST: Self = Self(0xC000_0010);
    pub(super) const ACCESS_DENIED: Self = Self(0xC000_0022);
    pub(super) const OBJECT_TYPE_MISMATCH: Self = Self(0xC000_0024);
    pub(super) const BUFFER_TOO_SMALL: Self = Self(0xC000_0023);
    pub(super) const OBJECT_NAME_NOT_FOUND: Self = Self(0xC000_0034);
    pub(super) const OBJECT_NAME_COLLISION: Self = Self(0xC000_0035);
    pub(super) const OBJECT_PATH_NOT_FOUND: Self = Self(0xC000_003A);
    pub(super) const MEDIA_WRITE_PROTECTED: Self = Self(0xC000_00A2);
    pub(super) const FILE_IS_A_DIRECTORY: Self = Self(0xC000_00BA);
    pub(super) const UNEXPECTED_IO_ERROR: Self = Self(0xC000_00E9);
    pub(super) const NOT_A_DIRECTORY: Self = Self(0xC000_0103);
    pub(super) const INVALID_PARAMETER: Self = Self(0xC000_000D);
    pub(super) const NO_MEMORY: Self = Self(0xC000_0017);
    pub(super) const TIMEOUT: Self = Self(0x0000_0102);
    pub(super) const INFO_LENGTH_MISMATCH: Self = Self(0xC000_0004);
    pub(super) const INVALID_HANDLE: Self = Self(0xC000_0008);
    pub(super) const CONFLICTING_ADDRESSES: Self = Self(0xC000_0018);
    pub(super) const MUTANT_NOT_OWNED: Self = Self(0xC000_0046);
    pub(super) const SEMAPHORE_LIMIT_EXCEEDED: Self = Self(0xC000_0047);
    pub(super) const FREE_VM_NOT_AT_BASE: Self = Self(0xC000_009F);
    pub(super) const MEMORY_NOT_ALLOCATED: Self = Self(0xC000_00A0);
    pub(super) const INSUFFICIENT_RESOURCES: Self = Self(0xC000_009A);
}

/// One import call: its arguments, and the guest memory they point into.
pub(super) struct ImportCall {
    ctx: *mut GuestContext,
    pub(super) memory: &'static GuestMemory,
}

impl ImportCall {
    /// Guest arguments are 32-bit values in 64-bit registers; the upper half is not meaningful.
    pub(super) fn argument(&self, index: u32) -> u32 {
        if index < REGISTER_ARGUMENT_COUNT {
            // SAFETY: ctx is the caller's live context.
            return unsafe { pure_ctx_argument(self.ctx, index) as u32 };
        }
        // SAFETY: as above.
        let stack_pointer = unsafe { pure_ctx_stack_pointer(self.ctx) };
        let slot = index - REGISTER_ARGUMENT_COUNT;
        self.memory
            .read_u32(stack_pointer + STACK_ARGUMENTS_OFFSET + slot * STACK_ARGUMENT_SLOT_SIZE)
    }

    pub(super) fn return_address(&self) -> u32 {
        // SAFETY: as above.
        unsafe { pure_ctx_return_address(self.ctx) }
    }

    fn logged_arguments(&self) -> String {
        (0..LOGGED_ARGUMENT_COUNT)
            .map(|index| format!("r{}={:#x}", index + 3, self.argument(index)))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// BOOLEAN return values.
const TRUE: u32 = 1;
const FALSE: u32 = 0;

/// What a handler for a `void` import returns; r3 is not meaningful to the caller.
const NO_RETURN_VALUE: u32 = 0;

/// Runs an import and returns the value for r3. Most imports return a 32-bit value; a few
/// return a 64-bit LARGE_INTEGER, which fills the whole register.
#[derive(Clone, Copy)]
enum ImportHandler {
    Word(fn(&ImportCall) -> u32),
    DoubleWord(fn(&ImportCall) -> u64),
}

impl ImportHandler {
    fn run(self, call: &ImportCall) -> u64 {
        match self {
            Self::Word(handler) => u64::from(handler(call)),
            Self::DoubleWord(handler) => handler(call),
        }
    }
}

fn handler(name: &str) -> Option<ImportHandler> {
    use ImportHandler::{DoubleWord, Word};
    Some(match name {
        "ExCreateThread" => Word(threads::ex_create_thread),
        "ExGetXConfigSetting" => Word(system::get_xconfig_setting),
        "ExRegisterTitleTerminateNotification" => {
            Word(system::register_title_terminate_notification)
        }
        "ExTerminateThread" => Word(threads::ex_terminate_thread),
        "KeAcquireSpinLockAtRaisedIrql" => Word(irql::ke_acquire_spin_lock_at_raised_irql),
        "KeDelayExecutionThread" => Word(threads::ke_delay_execution_thread),
        "KeEnableFpuExceptions" => Word(threads::ke_enable_fpu_exceptions),
        "KeEnterCriticalRegion" => Word(threads::ke_critical_region),
        "KeGetCurrentProcessType" => Word(system::current_process_type),
        "KeInitializeSemaphore" => Word(objects::ke_initialize_semaphore),
        "KeLeaveCriticalRegion" => Word(threads::ke_critical_region),
        "KeQueryBasePriorityThread" => Word(threads::ke_query_base_priority_thread),
        "KeQueryPerformanceFrequency" => DoubleWord(|_| timebase::frequency()),
        "KeQuerySystemTime" => Word(system::query_system_time),
        "KeRaiseIrqlToDpcLevel" => Word(irql::ke_raise_irql_to_dpc_level),
        "KeReleaseSemaphore" => Word(objects::ke_release_semaphore),
        "KeReleaseSpinLockFromRaisedIrql" => Word(irql::ke_release_spin_lock_from_raised_irql),
        "KeResetEvent" => Word(objects::ke_reset_event),
        "KeResumeThread" => Word(threads::ke_resume_thread),
        "KeSetAffinityThread" => Word(threads::ke_set_affinity_thread),
        "KeSetBasePriorityThread" => Word(threads::ke_set_base_priority_thread),
        "KeSetDisableBoostThread" => Word(threads::ke_set_disable_boost_thread),
        "KeSetEvent" => Word(objects::ke_set_event),
        "KeTlsAlloc" => Word(tls::alloc),
        "KeTlsFree" => Word(tls::free),
        "KeTlsGetValue" => Word(tls::get_value),
        "KeTlsSetValue" => Word(tls::set_value),
        "KeTryToAcquireSpinLockAtRaisedIrql" => {
            Word(irql::ke_try_to_acquire_spin_lock_at_raised_irql)
        }
        "KeWaitForMultipleObjects" => Word(objects::ke_wait_for_multiple_objects),
        "KeWaitForSingleObject" => Word(objects::ke_wait_for_single_object),
        "KfAcquireSpinLock" => Word(irql::kf_acquire_spin_lock),
        "KfLowerIrql" => Word(irql::kf_lower_irql),
        "KfReleaseSpinLock" => Word(irql::kf_release_spin_lock),
        "KiApcNormalRoutineNop" => Word(system::apc_normal_routine_nop),
        "MmAllocatePhysicalMemory" => Word(memory::mm_allocate_physical_memory),
        "MmAllocatePhysicalMemoryEx" => Word(memory::mm_allocate_physical_memory_ex),
        "MmFreePhysicalMemory" => Word(memory::mm_free_physical_memory),
        "MmGetPhysicalAddress" => Word(memory::mm_get_physical_address),
        "MmQueryAddressProtect" => Word(memory::mm_query_address_protect),
        "NetDll_XNetGetEthernetLinkStatus" => Word(network::xnet_get_ethernet_link_status),
        "NetDll_XNetGetTitleXnAddr" => Word(network::xnet_get_title_xn_addr),
        "NtAllocateVirtualMemory" => Word(memory::nt_allocate_virtual_memory),
        "NtCancelTimer" => Word(timers::nt_cancel_timer),
        "NtClearEvent" => Word(objects::nt_clear_event),
        "NtClose" => Word(nt_close),
        "NtCreateEvent" => Word(objects::nt_create_event),
        "NtCreateFile" => Word(files::nt_create_file),
        "NtCreateMutant" => Word(objects::nt_create_mutant),
        "NtCreateSemaphore" => Word(objects::nt_create_semaphore),
        "NtCreateTimer" => Word(timers::nt_create_timer),
        "NtFlushBuffersFile" => Word(files::nt_flush_buffers_file),
        "NtFreeVirtualMemory" => Word(memory::nt_free_virtual_memory),
        "NtOpenFile" => Word(files::nt_open_file),
        "NtQueryDirectoryFile" => Word(files::nt_query_directory_file),
        "NtQueryFullAttributesFile" => Word(files::nt_query_full_attributes_file),
        "NtQueryInformationFile" => Word(files::nt_query_information_file),
        "NtReadFile" => Word(files::nt_read_file),
        "NtReleaseMutant" => Word(objects::nt_release_mutant),
        "NtReleaseSemaphore" => Word(objects::nt_release_semaphore),
        "NtResumeThread" => Word(threads::nt_resume_thread),
        "NtSetEvent" => Word(objects::nt_set_event),
        "NtSetTimerEx" => Word(timers::nt_set_timer_ex),
        "NtWaitForMultipleObjectsEx" => Word(objects::nt_wait_for_multiple_objects_ex),
        "NtWaitForSingleObjectEx" => Word(objects::nt_wait_for_single_object_ex),
        "NtWriteFile" => Word(files::nt_write_file),
        "NtYieldExecution" => Word(threads::nt_yield_execution),
        "ObDereferenceObject" => Word(objects::ob_dereference_object),
        "ObReferenceObjectByHandle" => Word(objects::ob_reference_object_by_handle),
        "RtlCompareMemory" => Word(rtl::compare_memory),
        "RtlCompareMemoryUlong" => Word(rtl::compare_memory_ulong),
        "RtlEnterCriticalSection" => Word(rtl::enter_critical_section),
        "RtlFillMemoryUlong" => Word(rtl::fill_memory_ulong),
        "RtlImageXexHeaderField" => Word(xex::image_xex_header_field),
        "RtlInitAnsiString" => Word(rtl::init_ansi_string),
        "RtlInitUnicodeString" => Word(rtl::init_unicode_string),
        "RtlInitializeCriticalSection" => Word(rtl::initialize_critical_section),
        "RtlInitializeCriticalSectionAndSpinCount" => {
            Word(rtl::initialize_critical_section_and_spin_count)
        }
        "RtlLeaveCriticalSection" => Word(rtl::leave_critical_section),
        "RtlNtStatusToDosError" => Word(rtl::nt_status_to_dos_error),
        "RtlRaiseException" => Word(threads::rtl_raise_exception),
        "RtlTimeFieldsToTime" => Word(rtl::time_fields_to_time),
        "RtlTimeToTimeFields" => Word(rtl::time_to_time_fields),
        "RtlTryEnterCriticalSection" => Word(rtl::try_enter_critical_section),
        "VdCallGraphicsNotificationRoutines" => Word(video::call_graphics_notification_routines),
        "VdEnableRingBufferRPtrWriteBack" => Word(video::enable_ring_buffer_read_pointer_writeback),
        "VdGetCurrentDisplayGamma" => Word(video::get_current_display_gamma),
        "VdGetCurrentDisplayInformation" => Word(video::get_current_display_information),
        "VdGetSystemCommandBuffer" => Word(video::get_system_command_buffer),
        "VdInitializeEngines" => Word(video::initialize_engines),
        "VdInitializeRingBuffer" => Word(video::initialize_ring_buffer),
        "VdInitializeScalerCommandBuffer" => Word(video::initialize_scaler_command_buffer),
        "VdIsHSIOTrainingSucceeded" => Word(video::is_hsio_training_succeeded),
        "VdPersistDisplay" => Word(video::persist_display),
        "VdQueryVideoFlags" => Word(video::query_video_flags),
        "VdQueryVideoMode" => Word(video::query_video_mode),
        "VdRetrainEDRAM" => Word(video::retrain_edram),
        "VdRetrainEDRAMWorker" => Word(video::retrain_edram),
        "VdSetDisplayMode" => Word(video::set_display_mode),
        "VdSetGraphicsInterruptCallback" => Word(video::set_graphics_interrupt_callback),
        "VdSetSystemCommandBufferGpuIdentifierAddress" => {
            Word(video::set_system_command_buffer_gpu_identifier_address)
        }
        "VdSwap" => Word(video::swap),
        "XAudioGetSpeakerConfig" => Word(audio::get_speaker_config),
        "XAudioGetVoiceCategoryVolume" => Word(audio::get_voice_category_volume),
        "XAudioGetVoiceCategoryVolumeChangeMask" => {
            Word(audio::get_voice_category_volume_change_mask)
        }
        "XAudioRegisterRenderDriverClient" => Word(audio::register_render_driver_client),
        "XAudioSubmitRenderDriverFrame" => Word(audio::submit_render_driver_frame),
        "XAudioUnregisterRenderDriverClient" => Word(audio::unregister_render_driver_client),
        "XGetAVPack" => Word(system::av_pack),
        "XGetGameRegion" => Word(system::game_region),
        "XGetVideoMode" => Word(video::query_video_mode),
        "XMACreateContext" => Word(xma::create_context),
        "XMADisableContext" => Word(xma::disable_context),
        "XMAEnableContext" => Word(xma::enable_context),
        "XMAGetInputBufferReadOffset" => Word(xma::get_input_buffer_read_offset),
        "XMAGetOutputBufferReadOffset" => Word(xma::get_output_buffer_read_offset),
        "XMAGetOutputBufferWriteOffset" => Word(xma::get_output_buffer_write_offset),
        "XMAInitializeContext" => Word(xma::initialize_context),
        "XMAIsInputBuffer0Valid" => Word(xma::is_input_buffer_0_valid),
        "XMAIsInputBuffer1Valid" => Word(xma::is_input_buffer_1_valid),
        "XMAIsOutputBufferValid" => Word(xma::is_output_buffer_valid),
        "XMAReleaseContext" => Word(xma::release_context),
        "XMASetInputBuffer0Valid" => Word(xma::set_input_buffer_0_valid),
        "XMASetInputBuffer1Valid" => Word(xma::set_input_buffer_1_valid),
        "XMASetInputBufferReadOffset" => Word(xma::set_input_buffer_read_offset),
        "XMASetOutputBufferReadOffset" => Word(xma::set_output_buffer_read_offset),
        "XMASetOutputBufferValid" => Word(xma::set_output_buffer_valid),
        "XMsgInProcessCall" => Word(xmsg::in_process_call),
        "XMsgStartIORequest" => Word(xmsg::start_io_request),
        "XMsgStartIORequestEx" => Word(xmsg::start_io_request_ex),
        "XNotifyGetNext" => Word(notify::get_next),
        "XamContentClose" => Word(content::close),
        "XamContentCreateEnumerator" => Word(content::create_enumerator),
        "XamContentCreateEx" => Word(content::create_ex),
        "XamContentGetDeviceData" => Word(content::get_device_data),
        "XamEnumerate" => Word(content::enumerate),
        "XamGetSystemVersion" => Word(system::system_version),
        "XamInputGetState" => Word(input::get_state),
        "XamInputSetState" => Word(input::set_state),
        "XamNotifyCreateListener" => Word(notify::create_listener),
        "XamShowDeviceSelectorUI" => Word(content::show_device_selector_ui),
        "XamUserCheckPrivilege" => Word(user::check_privilege),
        "XamUserGetName" => Word(user::get_name),
        "XamUserGetSigninInfo" => Word(user::get_signin_info),
        "XamUserGetSigninState" => Word(user::get_signin_state),
        "XamUserGetXUID" => Word(user::get_xuid),
        "XamUserReadProfileSettings" => Word(user::read_profile_settings),
        "XexCheckExecutablePrivilege" => Word(xex::check_executable_privilege),
        _ => return None,
    })
}

/// Called by the generated import forwarders on the guest thread.
#[unsafe(no_mangle)]
extern "C" fn pure_import_call(name: *const c_char, ctx: *mut GuestContext, _base: *mut u8) {
    // SAFETY: the forwarders pass string literals and the live guest context.
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    let call = ImportCall {
        ctx,
        memory: guest::memory(),
    };
    let arguments = call.logged_arguments();
    let return_address = call.return_address();

    let Some(handler) = handler(&name) else {
        info!(target: "import", "{name}({arguments}) from {return_address:#010x}");
        status::halt_guest_thread(format!(
            "unimplemented import {name} called from {return_address:#010x}"
        ));
    };
    let result = handler.run(&call);
    let thread = guest::current_thread();
    debug!(target: "import", "{name}({arguments}) from {return_address:#010x} -> {result:#x} [thread {thread:#010x}]");
    // SAFETY: ctx is the caller's live context.
    unsafe { pure_ctx_set_return(ctx, result) };
}

/// ANSI_STRING: the length in bytes, then (after the capacity) the buffer pointer.
const ANSI_STRING_LENGTH: u32 = 0x0;
const ANSI_STRING_BUFFER: u32 = 0x4;

/// An ANSI_STRING's text. Guest names are ANSI; a non-ASCII byte cannot match a disc file or
/// object name the runtime knows anyway, so lossy conversion is harmless.
fn read_ansi_string(call: &ImportCall, string: u32) -> String {
    let length = call.memory.read_u16(string + ANSI_STRING_LENGTH);
    let buffer = call.memory.read_u32(string + ANSI_STRING_BUFFER);
    let bytes: Vec<u8> = (0..u32::from(length))
        .map(|offset| call.memory.read_u8(buffer + offset))
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// NtClose(handle) -> NTSTATUS.
fn nt_close(call: &ImportCall) -> u32 {
    match HANDLES.lock().unwrap().remove(call.argument(0)) {
        Some(_) => NtStatus::SUCCESS.0,
        None => NtStatus::INVALID_HANDLE.0,
    }
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
