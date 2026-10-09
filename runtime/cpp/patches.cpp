// Fixes for races in the title's own code that the console's timing hides and the host's does
// not. Each fix replaces one recompiled function (the generated definitions are weak); the
// original stays callable as __imp__sub_<address>.

#include "host.h"

#include <atomic>
#include <cstdint>
#include <mutex>
#include <unordered_map>

// The functions below that replace recompiled ones.
constexpr uint32_t kTitleFixCount = 3;

// Logged by the runtime at start-up (src/guest.rs).
PURE_EXPORT uint32_t pure_title_fix_count()
{
    return kTitleFixCount;
}

// The generated implementations have C linkage (PPC_FUNC_IMPL).
extern "C" PPC_FUNC(__imp__sub_82528108);
extern "C" PPC_FUNC(__imp__sub_8252A978);

namespace {

// The title's job graph: the main thread appends records and activates the graph, job threads
// run the records it has issued, and the thread that completes the last one deactivates it.
constexpr uint32_t kGraphStream = 4;
constexpr uint32_t kGraphRecordCount = 24;
constexpr uint32_t kGraphActive = 45;
constexpr uint32_t kGraphCompletedCount = 60;
constexpr uint8_t kActive = 1;

// A job stream: the graphs (queues) its job threads take records from, where their scan starts,
// the condition variable they wait on for work, and whether the stream is stopping.
constexpr uint32_t kStreamQueueCount = 8;
constexpr uint32_t kStreamQueues = 12;
constexpr uint32_t kStreamScanStart = 28;
constexpr uint32_t kStreamWork = 32;
constexpr uint32_t kStreamStopping = 36;
constexpr uint32_t kWordSize = 4;
// The frame the job thread's step keeps on the guest stack, as the original does, and the lock
// guard in it.
constexpr uint32_t kStepFrameSize = 144;
constexpr uint32_t kStepLockGuard = 80;

// How many times each graph has been activated, keyed by its guest address.
std::mutex activationsLock;
std::unordered_map<uint32_t, uint64_t> activations;

uint64_t Activations(uint32_t graph)
{
    std::lock_guard lock(activationsLock);
    return activations[graph];
}

struct IssuedRecord
{
    uint32_t graph;
    uint32_t record;
};

// Takes the next issued record from the stream's graphs, scanning them in the order the
// original does; a record is 0 when none has one.
IssuedRecord TakeIssuedRecord(PPCContext& ctx, uint8_t* base, uint32_t stream)
{
    const uint32_t start = PPC_LOAD_U32(stream + kStreamScanStart);
    const uint32_t count = PPC_LOAD_U32(stream + kStreamQueueCount);
    const uint32_t queues = PPC_LOAD_U32(stream + kStreamQueues);
    for (uint32_t index = start + 1; index < start + count + 1; ++index)
    {
        const uint32_t graph = PPC_LOAD_U32(queues + index % count * kWordSize);
        ctx.r3.u64 = graph;
        sub_821D3950(ctx, base);
        if (ctx.r3.u32 != 0)
            return { graph, ctx.r3.u32 };
    }
    return { 0, 0 };
}

void RunRecord(PPCContext& ctx, uint8_t* base, IssuedRecord issued, uint32_t argument)
{
    ctx.r3.u64 = issued.graph;
    ctx.r4.u64 = issued.record;
    ctx.r5.u64 = argument;
    sub_82272D90(ctx, base);
}

}

// Activates a job graph: issues the records appended since, and wakes the job threads.
PPC_FUNC(sub_82528108)
{
    {
        std::lock_guard lock(activationsLock);
        ++activations[ctx.r3.u32];
    }
    __imp__sub_82528108(ctx, base);
}

// Completes one record of a job graph, deactivating the graph if it was the last. That check
// reads the record count, while the main thread appends records and activates the graph without
// the graph's lock, so a job thread held up between the check and the deactivation clears the
// flag the main thread has just set, and the new records never run: the main thread then waits
// for them forever. On the console the window is a few instructions; on the host, a preempted
// thread can sit in it for a whole time slice. An activation during the completion that left
// the graph inactive with records outstanding is that lost activation, so it is redone.
PPC_FUNC(sub_8252A978)
{
    const uint32_t graph = ctx.r3.u32;
    const uint64_t activationsBefore = Activations(graph);
    __imp__sub_8252A978(ctx, base);
    std::atomic_thread_fence(std::memory_order_seq_cst);
    const bool lostActivation = PPC_LOAD_U8(graph + kGraphActive) != kActive
        && Activations(graph) != activationsBefore
        && PPC_LOAD_U32(graph + kGraphRecordCount) != PPC_LOAD_U32(graph + kGraphCompletedCount);
    if (lostActivation)
    {
        PPC_STORE_U8(graph + kGraphActive, kActive);
        // Wake the stream's job threads, as activating the graph does.
        ctx.r3.u32 = PPC_LOAD_U32(graph + kGraphStream);
        sub_822A2F50(ctx, base);
    }
}

// One step of a job thread: runs an issued record, or waits for the stream to issue more. The
// original scans the graphs before taking the lock its wait uses, while activation takes that
// lock only to notify, so a record issued between a thread's scan and its wait is never seen,
// and once every job thread has missed it the main thread waits for it forever. As above, the
// console's timing keeps threads out of that window and the host's does not. Scanning again
// under the lock closes it.
PPC_FUNC(sub_8252B1C8)
{
    const uint32_t stream = ctx.r3.u32;
    const uint32_t argument = ctx.r4.u32;
    if (PPC_LOAD_U8(stream + kStreamStopping) == 0)
    {
        IssuedRecord issued = TakeIssuedRecord(ctx, base, stream);
        if (issued.record == 0)
        {
            const uint32_t caller_frame = ctx.r1.u32;
            ctx.r1.u64 = caller_frame - kStepFrameSize;
            PPC_STORE_U32(ctx.r1.u32, caller_frame);
            const uint32_t guard = ctx.r1.u32 + kStepLockGuard;
            const uint32_t work = PPC_LOAD_U32(stream + kStreamWork);
            ctx.r3.u64 = guard;
            ctx.r4.u64 = work;
            sub_822A1F00(ctx, base);
            ctx.r3.u64 = guard;
            sub_82214760(ctx, base);
            issued = TakeIssuedRecord(ctx, base, stream);
            if (issued.record == 0)
            {
                ctx.r3.u64 = work;
                ctx.r4.u64 = guard;
                sub_82236AA0(ctx, base);
            }
            ctx.r3.u64 = guard;
            sub_82203DA0(ctx, base);
            ctx.r1.u64 = caller_frame;
        }
        if (issued.record != 0)
            RunRecord(ctx, base, issued, argument);
    }
    ctx.r3.u64 = PPC_LOAD_U8(stream + kStreamStopping);
}
