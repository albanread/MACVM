# macOS JIT Buffer: Enabling Executable Memory on Apple Silicon

On macOS (especially Apple Silicon), you cannot simply `mmap` a page as read-write, emit code into it, `mprotect` it to read-execute, and run it. Apple enforces **W^X (Write XOR Execute)** — a page cannot be simultaneously writable and executable through traditional `mprotect`. The mechanism for JIT compilers is a **per-thread** write/execute toggle on specially allocated memory.

---

## TL;DR — The 4-Step Process

| Step | API | Purpose |
|------|-----|---------|
| **Allocate** | `mmap(..., MAP_JIT)` | Get memory the kernel allows to be both writable and executable |
| **Write** | `pthread_jit_write_protect_np(0)` then write code | Switch this thread to write mode, emit machine code |
| **Finalize** | `pthread_jit_write_protect_np(1)` then `sys_icache_invalidate()` | Switch back to execute mode, flush I-cache |
| **Skip mprotect** | Check `IsMapJitRegion()`, return early | Never call `mprotect` on MAP_JIT pages |

---

## 1. Allocation — `mmap` with `MAP_JIT`

Code pages must be allocated with the `MAP_JIT` flag:

```c
#include <sys/mman.h>

void* rawAddr = mmap(NULL, allocSize, PROT_READ | PROT_WRITE | PROT_EXEC,
                     MAP_PRIVATE | MAP_ANON | MAP_JIT, -1, 0);
```

Key details:

- You request `PROT_READ | PROT_WRITE | PROT_EXEC` — the kernel grants this **only** because `MAP_JIT` is present.
- `MAP_JIT` pages are **not** tracked by ChakraCore's PAL `VirtualQuery`, so a side table is needed to identify JIT regions. The project uses a simple fixed-size array:

```c
struct MapJitRegion {
    void* address;
    size_t size;
};
static const int kMaxMapJitRegions = 256;
static MapJitRegion s_mapJitRegions[kMaxMapJitRegions];
```

See: `lib/Common/Memory/VirtualAllocWrapper.cpp` — `RegisterMapJitRegion`, `UnregisterMapJitRegion`, `IsMapJitRegion`.

- macOS ARM64 pages are 16KB, but ChakraCore expects 64KB alignment (Windows convention), so the code over-allocates and trims leading/trailing bytes with `munmap`.
- `mprotect` **cannot be used** on `MAP_JIT` pages for toggling permissions — it will either fail or cause crashes. All such calls must be skipped for these regions.

### Entitlements

The binary **must** be signed with the `com.apple.security.cs.allow-jit` entitlement for `MAP_JIT` to work. Without it, the `mmap` call will fail.

---

## 2. Writing Code — `pthread_jit_write_protect_np()`

W^X on `MAP_JIT` pages is toggled **per-thread**, not per-page. A thread is either in **write mode** (can write to JIT pages, cannot execute them) or **execute mode** (can execute JIT pages, cannot write to them). The default for new threads is execute mode.

```c
#include <pthread.h>

pthread_jit_write_protect_np(0);  // Enable WRITE mode (disable execute)
pthread_jit_write_protect_np(1);  // Enable EXECUTE mode (disable write)
```

### Thread Strategy

The project uses two strategies depending on the thread:

#### Background JIT thread — permanently in write mode

It only compiles, never executes JIT code:

```c
// In BackgroundJobProcessor::StaticThreadProc()
#if defined(__APPLE__) && defined(_M_ARM64)
    // Keep background JIT thread permanently in write mode so that
    // all writes to MAP_JIT pages succeed without per-write toggling.
    pthread_jit_write_protect_np(0);
#endif
```

See: `lib/Common/Common/Jobs.cpp`

#### Main thread (foreground JIT) — toggle around codegen

Toggle to write mode before code generation, back to execute mode after:

```c
// In NativeCodeGenerator::CodeGen()
#if defined(__APPLE__) && defined(_M_ARM64)
    if (foreground)
    {
        pthread_jit_write_protect_np(0); // Enable writing for code generation
    }
#endif

    // ... emit code ...

#if defined(__APPLE__) && defined(_M_ARM64)
    if (foreground)
    {
        pthread_jit_write_protect_np(1); // Switch back to execute mode
    }
#endif
```

See: `lib/Backend/NativeCodeGenerator.cpp`

#### Interpreter thunk emission — bracket writes with toggle on/off

Every error path must also restore execute mode:

```c
// In InterpreterThunkEmitter::NewThunkBlock()
#if defined(__APPLE__) && defined(_M_ARM64)
    pthread_jit_write_protect_np(0); // Enable write mode
#endif

    EmitBufferAllocation* allocation = emitBufferManager.AllocateBuffer(BlockSize, &buffer);
    if (allocation == nullptr)
    {
#if defined(__APPLE__) && defined(_M_ARM64)
        pthread_jit_write_protect_np(1); // Restore execute mode on error
#endif
        Js::Throw::OutOfMemory();
    }

    // ... write code to buffer ...

#if defined(__APPLE__) && defined(_M_ARM64)
    pthread_jit_write_protect_np(1); // Switch back to execute mode
#endif
```

See: `lib/Backend/InterpreterThunkEmitter.cpp`

---

## 3. Making Code Executable — `sys_icache_invalidate()`

After writing code and switching back to execute mode, you **must flush the instruction cache**. ARM64 has separate I-cache and D-cache; writes go through D-cache but execution reads from I-cache. Without a flush, the CPU may execute stale or garbage data.

```c
#include <libkern/OSCacheControl.h>

sys_icache_invalidate(allocation->address, allocation->size);
```

This is done when transitioning a page to execute-only ("finalizing" code):

```c
// In CustomHeap::ProtectAllocationWithExecuteReadOnly()
#if defined(__APPLE__) && defined(_M_ARM64)
    if (VirtualAllocWrapper::IsMapJitRegion(allocation->address))
    {
        sys_icache_invalidate(allocation->address, allocation->size);
        return TRUE;
    }
#endif
```

It is also done after filling pages with debug break instructions (so the I-cache sees the BRK instructions):

```c
// In CustomHeap::AllocNewPage() and AllocLargeObject()
FillDebugBreak((BYTE*)localAddr, AutoSystemInfo::PageSize);
#if defined(__APPLE__) && defined(_M_ARM64)
    if (VirtualAllocWrapper::IsMapJitRegion(address))
    {
        sys_icache_invalidate(localAddr, AutoSystemInfo::PageSize);
    }
#endif
```

See: `lib/Common/Memory/CustomHeap.cpp`

---

## 4. Bypassing `mprotect` for MAP_JIT Pages

Every place that would normally call `mprotect` / `VirtualProtect` to change page permissions must **skip the call** for `MAP_JIT` regions. Those pages are always kernel-level RWX; per-thread access is controlled exclusively by `pthread_jit_write_protect_np`:

```c
// In CustomHeap::ProtectAllocationWithExecuteReadWrite()
#if defined(__APPLE__) && defined(_M_ARM64)
    // W^X is managed per-thread, not per-page. Skip mprotect.
    if (VirtualAllocWrapper::IsMapJitRegion(allocation->address))
    {
        return TRUE;
    }
#endif
```

The same guard appears in:
- `CustomHeap::ProtectAllocationWithExecuteReadOnly` — skip mprotect, just flush I-cache
- `CustomHeap::AllocLargeObject` — skip mprotect after allocation
- `CustomHeap::AllocNewPage` — skip mprotect for new pages
- `CustomHeap::FreeAllocation` — skip mprotect when returning pages
- `CustomHeap::FreeAllocationHelper` — skip mprotect, flush I-cache after writing debug breaks
- `HeapPageAllocator::ProtectPages` — skip VirtualProtect and VirtualQuery
- `EmitBufferManager::AllocateBuffer` — relax VirtualQuery assertion

See: `lib/Common/Memory/CustomHeap.cpp`, `lib/Common/Memory/PageAllocator.cpp`, `lib/Backend/EmitBuffer.cpp`

---

## Relevant Source Files

| File | Role |
|------|------|
| `lib/Common/Memory/VirtualAllocWrapper.cpp` | `MAP_JIT` allocation, region tracking |
| `lib/Common/Memory/VirtualAllocWrapper.h` | `RegisterMapJitRegion` / `IsMapJitRegion` API |
| `lib/Common/Memory/CustomHeap.cpp` | mprotect bypass, I-cache flush |
| `lib/Common/Memory/PageAllocator.cpp` | mprotect bypass for page protection |
| `lib/Common/Common/Jobs.cpp` | Background JIT thread permanent write mode |
| `lib/Backend/NativeCodeGenerator.cpp` | Foreground JIT write/execute toggling |
| `lib/Backend/InterpreterThunkEmitter.cpp` | Thunk emission write/execute toggling |
| `lib/Backend/EmitBuffer.cpp` | VirtualQuery assertion relaxation |
| `lib/Backend/arm64/AppleSilicon/AppleSiliconConfig.h` | Configuration and constraints summary |

---

## Required Headers

```c
#include <sys/mman.h>           // mmap, munmap, MAP_JIT, MAP_PRIVATE, MAP_ANON
#include <pthread.h>            // pthread_jit_write_protect_np
#include <libkern/OSCacheControl.h>  // sys_icache_invalidate
```

---

## Common Pitfalls

1. **Forgetting `MAP_JIT`** — `mmap` with `PROT_EXEC` will fail without it on hardened runtime.
2. **Calling `mprotect` on MAP_JIT pages** — This will fail or crash. Must be skipped entirely.
3. **Missing I-cache flush** — Code will appear to work intermittently, executing stale cache lines.
4. **Not restoring execute mode on error paths** — If `pthread_jit_write_protect_np(1)` is skipped due to an early return/throw, subsequent JIT code execution on that thread will fault.
5. **Executing JIT code on a write-mode thread** — The background JIT thread must never call into JIT code.
6. **Missing entitlement** — The `com.apple.security.cs.allow-jit` entitlement must be present in the code signature.
7. **Alignment assumptions** — macOS ARM64 pages are 16KB, not 4KB. If your engine assumes 64KB allocation granularity (Windows convention), you must over-allocate and trim.