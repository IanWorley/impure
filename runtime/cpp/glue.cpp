// Bridge between XenonRecomp's generated C++ and the Rust runtime.
// Contains no game code: it maps the user's own executable into guest memory, fills the
// guest-address -> host-function table, and starts guest code with a prepared context.

#include "ppc_recomp_shared.h"

#include <image.h>
#include <xex.h>

#include <cstring>
#include <string>
#include <string_view>
#include <unordered_map>
#include <vector>

// Ordinal -> name tables defined in XenonUtils' xex.cpp.
extern std::unordered_map<size_t, const char*> XamExports;
extern std::unordered_map<size_t, const char*> XboxKernelExports;

namespace {

// An import record holds ordinal (bits 0-15) and type (bits 24-31); type 0 is a pointer slot.
constexpr uint32_t kImportOrdinalMask = 0xFFFF;
constexpr uint32_t kImportTypeShift = 24;
constexpr uint32_t kSlotRecordType = 0;
constexpr size_t kImportNameAlignment = 4;
// XenonUtils overwrites each function's code thunk with nop; nop; nop; blr (host-order words).
constexpr uint32_t kRewrittenThunk[] = { 0x00000060, 0x00000060, 0x00000060, 0x2000804E };
constexpr std::string_view kSymbolPrefix = "__imp__";
// The lookup table has one slot per instruction in the code range.
constexpr uint32_t kInstructionSize = 4;

bool IsRewrittenThunk(const Image& image, uint32_t address)
{
    return std::memcmp(image.Find(address), kRewrittenThunk, sizeof(kRewrittenThunk)) == 0;
}

std::string ImportName(std::string_view library, const std::unordered_map<size_t, const char*>* names, uint32_t ordinal)
{
    if (names != nullptr)
    {
        if (auto name = names->find(ordinal); name != names->end())
        {
            std::string_view symbol = name->second;
            if (symbol.starts_with(kSymbolPrefix))
                symbol.remove_prefix(kSymbolPrefix.size());
            return std::string(symbol);
        }
    }
    return std::string(library) + "#" + std::to_string(ordinal);
}

}

extern "C" {

// Implemented in Rust (src/kernel.rs): returns the guest address the slot should point to.
uint32_t pure_resolve_variable_import(const char* name);
// Implemented in Rust (src/kernel.rs): reports the call and stops the guest thread.
[[noreturn]] void pure_unmapped_call_halt(uint32_t target, uint32_t return_address);

// Every lookup-table slot without a recompiled function points here, so an indirect call to such
// an address stops the guest with a report instead of jumping through a null host pointer.
// Indirect calls go through ctr, which therefore holds the target.
PPC_FUNC(pure_unmapped_call)
{
    pure_unmapped_call_halt(ctx.ctr.u32, uint32_t(ctx.lr));
}

// Copies the executable's image into guest memory and resolves import slots.
// Returns the entry point, or 0 if the executable does not match the generated code.
uint32_t pure_guest_load(const uint8_t* xex, size_t xex_size, uint8_t* base)
{
    Image image = Image::ParseImage(xex, xex_size);
    if (image.data == nullptr || image.base != PPC_IMAGE_BASE || image.size != PPC_IMAGE_SIZE)
        return 0;

    std::memcpy(base + image.base, image.data.get(), image.size);

    for (uint32_t address = PPC_CODE_BASE; address < PPC_CODE_BASE + PPC_CODE_SIZE; address += kInstructionSize)
        PPC_LOOKUP_FUNC(base, address) = pure_unmapped_call;
    for (const PPCFuncMapping* mapping = PPCFuncMappings; mapping->host != nullptr; ++mapping)
        PPC_LOOKUP_FUNC(base, mapping->guest) = mapping->host;

    // A function import has a pointer slot followed by its code thunk; the generated code calls
    // the thunk's import forwarder directly, and the slot is pointed at the thunk for any code
    // that calls through it. A variable import has only the slot, which gets a runtime-owned
    // block of guest memory.
    const auto* imports = static_cast<const Xex2ImportHeader*>(getOptHeaderPtr(xex, XEX_HEADER_IMPORT_LIBRARIES));
    if (imports == nullptr)
        return uint32_t(image.entry_point);

    std::vector<std::string_view> libraryNames;
    const char* stringTable = reinterpret_cast<const char*>(imports + 1);
    size_t stringOffset = 0;
    for (size_t i = 0; i < imports->numImports; i++)
    {
        libraryNames.emplace_back(stringTable + stringOffset);
        const size_t length = libraryNames.back().length() + 1;
        stringOffset += (length + kImportNameAlignment - 1) & ~(kImportNameAlignment - 1);
    }

    auto* library = reinterpret_cast<const Xex2ImportLibrary*>(stringTable + imports->sizeOfStringTable);
    for (const std::string_view libraryName : libraryNames)
    {
        const auto* names = libraryName == "xam.xex" ? &XamExports
            : libraryName == "xboxkrnl.exe" ? &XboxKernelExports : nullptr;
        const auto* descriptors = reinterpret_cast<const Xex2ImportDescriptor*>(library + 1);

        for (size_t i = 0; i < library->numberOfImports; i++)
        {
            const uint32_t slotAddress = descriptors[i].firstThunk;
            if (IsRewrittenThunk(image, slotAddress))
                continue;

            // XenonUtils left the slot record byte-swapped to host order in its copy of the image.
            uint32_t record;
            std::memcpy(&record, image.Find(slotAddress), sizeof(record));
            if ((record >> kImportTypeShift) != kSlotRecordType)
                continue;

            const bool isFunction = i + 1 < library->numberOfImports
                && IsRewrittenThunk(image, descriptors[i + 1].firstThunk);
            if (isFunction)
            {
                PPC_STORE_U32(slotAddress, descriptors[i + 1].firstThunk);
                continue;
            }

            const std::string name = ImportName(libraryName, names, record & kImportOrdinalMask);
            PPC_STORE_U32(slotAddress, pure_resolve_variable_import(name.c_str()));
        }
        library = reinterpret_cast<const Xex2ImportLibrary*>(descriptors + library->numberOfImports);
    }

    return uint32_t(image.entry_point);
}

// Runs a guest function with two arguments to completion on the calling host thread, and
// returns its result (r3).
uint32_t pure_guest_run(uint8_t* base, uint32_t function, uint32_t stack_top, uint32_t pcr,
    uint32_t argument0, uint32_t argument1)
{
    PPCContext ctx{};
    ctx.r1.u64 = stack_top;
    ctx.r3.u64 = argument0;
    ctx.r4.u64 = argument1;
    ctx.r13.u64 = pcr;
    ctx.fpscr.loadFromHost();

    PPCFunc* host = PPC_LOOKUP_FUNC(base, function);
    host(ctx, base);
    return ctx.r3.u32;
}

// Register access for import handlers, which see the context as an opaque pointer.
// Only the registers the guest ABI uses for arguments and returns are exposed.
uint64_t pure_ctx_argument(const PPCContext* ctx, uint32_t index)
{
    switch (index)
    {
    case 0: return ctx->r3.u64;
    case 1: return ctx->r4.u64;
    case 2: return ctx->r5.u64;
    case 3: return ctx->r6.u64;
    case 4: return ctx->r7.u64;
    case 5: return ctx->r8.u64;
    case 6: return ctx->r9.u64;
    case 7: return ctx->r10.u64;
    default: return 0;
    }
}

void pure_ctx_set_return(PPCContext* ctx, uint64_t value)
{
    ctx->r3.u64 = value;
}

uint32_t pure_ctx_stack_pointer(const PPCContext* ctx)
{
    return ctx->r1.u32;
}

uint32_t pure_ctx_return_address(const PPCContext* ctx)
{
    return uint32_t(ctx->lr);
}

}
