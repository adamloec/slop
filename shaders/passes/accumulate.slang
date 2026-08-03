// A compute shader that writes a storage buffer, so the barrier that orders
// that write against a later read has something to order.
//
// `docs/PLAN.md` §9.5 E1c. `BufferState::storage_write` describes a compute pass
// writing through the heap's storage-buffer binding; asserting the access mask
// in a unit test only proves the mask was typed as typed. This is the consumer
// that makes it a claim about the GPU.
//
// The shape is §9.4's cluster build in miniature: one invocation per output
// element, each writing an entry a later pass indexes. What it computes is
// arbitrary; that it lands where the CPU expects is the point.

#include "lib/bindless.slang"

struct PushConstants
{
    // Which storage-buffer slot to write. The same heap index a fragment shader
    // would use to read it back, which is what the aliased writable view in
    // `lib/bindless.slang` exists to allow.
    uint target;

    // How many elements. The dispatch rounds up, so invocations past this must
    // write nothing rather than running off the end of the allocation.
    uint count;
};

[[vk::push_constant]] PushConstants g_push;

// 64 in one dimension, which is the shape a linear buffer wants: one wave
// covering 64 consecutive elements touches consecutive bytes. The 8x8 tile in
// `fill.slang` is right for an image and wrong here.
[shader("compute")]
[numthreads(64, 1, 1)]
void accumulateMain(uint3 id: SV_DispatchThreadID)
{
    if (id.x >= g_push.count)
    {
        return;
    }

    BufferHandle target = { g_push.target };

    // Index-derived rather than constant, so a shader writing every element to
    // the same place -- or writing element zero repeatedly -- fails rather than
    // producing a buffer that looks written.
    storeToBuffer<uint>(target, id.x, id.x * 3 + 1);
}
