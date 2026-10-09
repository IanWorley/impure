// The boundary between the game library (the generated code, this glue and the title fixes,
// built on the player's machine) and the runtime that loads it (src/game_module.rs).
// Contains no game code.
//
// The library calls into the runtime only through the table the runtime hands over with
// pure_attach_host, so it has no undefined symbols: a Windows DLL may not have any, and it keeps
// the library independent of how the runtime was linked.

#pragma once

#include "ppc_recomp_shared.h"

#include <cstdint>

#if defined(_WIN32)
#define PURE_EXPORT extern "C" __declspec(dllexport)
#else
#define PURE_EXPORT extern "C" __attribute__((visibility("default")))
#endif

// Changes whenever PureHost or an exported function's signature changes. The runtime refuses a
// library built against another version (src/game_module.rs, HOST_ABI_VERSION).
constexpr uint32_t kPureHostAbiVersion = 1;

struct PureHost
{
    // Runs the kernel or XAM import `name` for the calling guest thread (src/kernel/mod.rs).
    void (*import_call)(const char* name, PPCContext* ctx, uint8_t* base);
    // Returns the guest address a variable import's slot should point to.
    uint32_t (*resolve_variable_import)(const char* name);
    // Reports an indirect call to an address with no recompiled function and stops the guest
    // thread; never returns.
    void (*unmapped_call_halt)(uint32_t target, uint32_t return_address);
};

// Set once by pure_attach_host, before any guest code runs.
extern const PureHost* pure_host;
