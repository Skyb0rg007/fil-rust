/*
 * fil-rust runtime shim.
 *
 * This is "yolo" (non-Fil-C) code that runs next to libpizlo and exposes a
 * small, flat C API over the Fil-C runtime's inline internals. It is compiled
 * once per Fil-C flavor against that flavor's libc headers plus the runtime
 * headers of the exact Fil-C version, because the layout of runtime structures
 * (filc_thread in particular) depends on both.
 *
 * Every entry point that takes a filc_thread* must be called on that thread,
 * and (unless documented otherwise) while it is entered.
 *
 * Functions returning `int` return an FR_* status code. They never panic the
 * Fil-C runtime for conditions the Rust side can report as errors.
 */

#include "filc_runtime.h"
#include "filc_runtime_inlines.h"
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#define FR_ABI_VERSION 1u

#define FR_OK 0
#define FR_NULL_CAPABILITY 1
#define FR_SPECIAL 2
#define FR_FREED 3
#define FR_OUT_OF_BOUNDS 4
#define FR_READONLY 5
#define FR_MISALIGNED 6
#define FR_NOT_A_FUNCTION 7
#define FR_EXCEPTION 8
#define FR_BAD_SLOT 9

/* Natives exported by libpizlo but declared only in a generated header. */
filc_ptr filc_native_zclosure_new(filc_thread* my_thread, filc_ptr function_ptr, filc_ptr data_ptr);
bool filc_native_zthread_create2(filc_thread* my_thread, filc_ptr callback_ptr, filc_ptr arg_ptr,
                                 filc_ptr zthread_ptr, filc_ptr tid_ptr);

/* Callbacks into Rust, registered by fr_init. */
typedef void (*fr_thread_main_fn)(filc_thread* thread, int is_main);
typedef size_t (*fr_callback_fn)(filc_thread* thread, uint64_t id, size_t argument_size);

static fr_thread_main_fn g_thread_main;
static fr_callback_fn g_callback;

#define FR_DECLARE_FUNCTION_OBJECT(name, thunk) \
    static pizlonated_return_value thunk(filc_thread*, void*, size_t); \
    static const filc_function_object name = { \
        .object = { \
            .upper = (void*)&name.function, \
            .aux = FILC_AUX_CREATE( \
                FILC_OBJECT_FLAGS_CREATE( \
                    FILC_OBJECT_FLAG_GLOBAL | FILC_OBJECT_FLAG_READONLY, \
                    FILC_SPECIAL_TYPE_FUNCTION, \
                    0), \
                (void*)&name.function) \
        }, \
        .function = { \
            .fast_entrypoint = (void*)thunk, \
            .generic_entrypoint = thunk, \
            .signature = FILC_GENERIC_SIGNATURE \
        } \
    }

FR_DECLARE_FUNCTION_OBJECT(fr_main_object, fr_main_thunk);
FR_DECLARE_FUNCTION_OBJECT(fr_worker_object, fr_worker_thunk);
FR_DECLARE_FUNCTION_OBJECT(fr_callback_base_object, fr_callback_thunk);

static filc_ptr fr_function_object_ptr(const filc_function_object* object)
{
    return filc_ptr_create_with_object_and_ptr_and_manual_tracking(
        (filc_object*)&object->object, (void*)&object->function);
}

/* ---- Roots ----------------------------------------------------------------
 *
 * Long-lived references held by Rust (FilHandle) are stored in a two-level
 * table of GC pointer arrays. The directory is allocated on the Fil-C main
 * thread and tracked by the main thunk's native frame, which is never popped,
 * so it (and everything reachable from it) stays alive for the life of the
 * process. Chunks are reachable through the directory. */

#define FR_ROOT_CHUNK_SLOTS 1024u
#define FR_ROOT_DIR_SLOTS 4096u

static filc_ptr g_root_dir;

static void fr_roots_init(filc_thread* thread)
{
    filc_object* dir = filc_allocate(thread, FR_ROOT_DIR_SLOTS * sizeof(void*));
    g_root_dir = filc_ptr_create_with_object(thread, dir);
}

size_t fr_root_capacity(void)
{
    return (size_t)FR_ROOT_CHUNK_SLOTS * FR_ROOT_DIR_SLOTS;
}

/* Caller must serialize calls that may create a chunk (the Rust side holds its
   handle-table lock). */
int fr_root_set(filc_thread* thread, size_t index, filc_ptr value)
{
    if (index >= fr_root_capacity())
        return FR_OUT_OF_BOUNDS;
    size_t dir_index = index / FR_ROOT_CHUNK_SLOTS;
    size_t slot = index % FR_ROOT_CHUNK_SLOTS;
    filc_ptr chunk = filc_load_ptr(thread, g_root_dir, dir_index * sizeof(void*));
    if (!filc_ptr_ptr(chunk)) {
        if (filc_ptr_is_totally_null(value))
            return FR_OK;
        chunk = filc_ptr_create_with_object(
            thread, filc_allocate(thread, FR_ROOT_CHUNK_SLOTS * sizeof(void*)));
        filc_store_ptr(thread, g_root_dir, dir_index * sizeof(void*), chunk);
    }
    filc_store_ptr(thread, chunk, slot * sizeof(void*), value);
    return FR_OK;
}

int fr_root_get(filc_thread* thread, size_t index, filc_ptr* out)
{
    *out = filc_ptr_forge_null();
    if (index >= fr_root_capacity())
        return FR_OUT_OF_BOUNDS;
    filc_ptr chunk = filc_load_ptr(
        thread, g_root_dir, (index / FR_ROOT_CHUNK_SLOTS) * sizeof(void*));
    if (!filc_ptr_ptr(chunk))
        return FR_OK;
    *out = filc_load_ptr(thread, chunk, (index % FR_ROOT_CHUNK_SLOTS) * sizeof(void*));
    return FR_OK;
}

/* ---- Thunks ---------------------------------------------------------------- */

static pizlonated_return_value fr_main_thunk(filc_thread* thread, void* callee_lower,
                                             size_t argument_size)
{
    PAS_UNUSED_PARAM(callee_lower);
    PAS_UNUSED_PARAM(argument_size);
    FILC_DEFINE_CATCHING_FRAME("fil_rust_main");
    filc_native_frame native_frame;
    filc_push_frame(thread, frame);
    filc_push_native_frame(thread, &native_frame);
    fr_roots_init(thread);
    g_thread_main(thread, 1);
    filc_internal_panic(NULL, "fil-rust: main executor returned.");
}

static pizlonated_return_value fr_worker_thunk(filc_thread* thread, void* callee_lower,
                                               size_t argument_size)
{
    PAS_UNUSED_PARAM(callee_lower);
    PAS_UNUSED_PARAM(argument_size);
    FILC_DEFINE_CATCHING_FRAME("fil_rust_worker");
    filc_native_frame native_frame;
    filc_push_frame(thread, frame);
    filc_push_native_frame(thread, &native_frame);
    g_thread_main(thread, 0);
    filc_internal_panic(NULL, "fil-rust: worker executor returned.");
}

static pizlonated_return_value fr_callback_thunk(filc_thread* thread, void* callee_lower,
                                                 size_t argument_size)
{
    FILC_DEFINE_CATCHING_FRAME("fil_rust_callback");
    filc_native_frame native_frame;
    filc_push_frame(thread, frame);
    filc_push_native_frame(thread, &native_frame);

    filc_object* object = filc_object_for_lower(callee_lower);
    if (!object
        || filc_object_special_type(object) != FILC_SPECIAL_TYPE_FUNCTION
        || (filc_object_get_flags(object) & FILC_OBJECT_FLAG_GLOBAL))
        filc_safety_panic(NULL, "fil-rust: callback invoked without closure data.");
    filc_closure* closure = (filc_closure*)filc_object_special_payload_with_manual_tracking(object);
    filc_ptr data = filc_flight_ptr_load_with_manual_tracking(&closure->data_ptr);
    uint64_t id = (uint64_t)(uintptr_t)filc_ptr_ptr(data);

    size_t return_size = g_callback(thread, id, argument_size);

    filc_pop_native_frame(thread, &native_frame);
    filc_pop_frame(thread, frame);
    return pizlonated_return_value_create(false, return_size);
}

/* ---- Lifecycle ------------------------------------------------------------- */

unsigned fr_abi_version(void)
{
    return FR_ABI_VERSION;
}

size_t fr_sizeof_thread(void)
{
    return sizeof(filc_thread);
}

int fr_runtime_is_glibc_flavor(void)
{
    return PAS_GLIBC;
}

void fr_init(fr_thread_main_fn thread_main, fr_callback_fn callback)
{
    g_thread_main = thread_main;
    g_callback = callback;
}

/* A pizlonated getter for the program's `main`, passed to filc_start_program. */
filc_ptr fr_main_getter(filc_thread* thread, const filc_origin* origin)
{
    PAS_UNUSED_PARAM(thread);
    PAS_UNUSED_PARAM(origin);
    return fr_function_object_ptr(&fr_main_object);
}

filc_thread* fr_my_thread(void)
{
    return filc_get_my_thread();
}

/* Sanity check of the compiled-in filc_thread layout against the live thread. */
int fr_check_thread_layout(filc_thread* thread)
{
    return thread == filc_get_my_thread()
        && (thread->state & FILC_THREAD_STATE_ENTERED)
        && thread->tid == (unsigned)gettid()
        && thread->stack_limit != NULL;
}

void fr_enter(filc_thread* thread)
{
    filc_enter(thread);
}

/* Must be called entered. */
void fr_exit(filc_thread* thread)
{
    filc_exit(thread);
}

int fr_is_entered(filc_thread* thread)
{
    return filc_thread_is_entered(thread);
}

void fr_pollcheck(filc_thread* thread)
{
    filc_pollcheck(thread, NULL);
}

/* Runs `body(arg)` inside a fresh native frame; every object tracked while it
   runs is released when it returns. */
void fr_with_native_frame(filc_thread* thread, void (*body)(void*), void* arg)
{
    filc_native_frame native_frame;
    filc_push_native_frame(thread, &native_frame);
    body(arg);
    filc_pop_native_frame(thread, &native_frame);
}

int fr_spawn_worker(filc_thread* thread)
{
    return filc_native_zthread_create2(
        thread, fr_function_object_ptr(&fr_worker_object), filc_ptr_forge_null(),
        filc_ptr_forge_null(), filc_ptr_forge_null()) ? FR_OK : FR_EXCEPTION;
}

PAS_NO_RETURN void fr_panic(const char* message)
{
    filc_user_panic(NULL, "%s", message);
}

PAS_NO_RETURN void fr_args_check_failure(size_t actual_size, size_t expected_size)
{
    filc_cc_args_check_failure(actual_size, expected_size, NULL);
}

/* ---- Pointers and memory ------------------------------------------------- */

typedef struct {
    void* lower;
    void* upper;
    unsigned flags;
    unsigned special_type;
} fr_ptr_info;

int fr_ptr_info_get(filc_ptr ptr, fr_ptr_info* out)
{
    memset(out, 0, sizeof(*out));
    filc_object* object = filc_ptr_object(ptr);
    if (!object)
        return FR_NULL_CAPABILITY;
    out->lower = filc_ptr_lower(ptr);
    out->upper = filc_object_upper(object);
    out->flags = (unsigned)filc_object_get_flags(object);
    out->special_type = (unsigned)filc_object_special_type(object);
    return FR_OK;
}

static int fr_check_access(filc_ptr ptr, size_t size, int write)
{
    filc_object* object = filc_ptr_object(ptr);
    if (!object)
        return FR_NULL_CAPABILITY;
    filc_object_flags flags = filc_object_get_flags(object);
    if (flags & FILC_OBJECT_FLAG_FREE)
        return FR_FREED;
    if (filc_object_special_type(object) != FILC_SPECIAL_TYPE_NONE)
        return FR_SPECIAL;
    uintptr_t p = (uintptr_t)filc_ptr_ptr(ptr);
    uintptr_t lower = (uintptr_t)filc_ptr_lower(ptr);
    uintptr_t upper = (uintptr_t)filc_object_upper(object);
    if (p < lower || p > upper || size > upper - p)
        return FR_OUT_OF_BOUNDS;
    if (write && (flags & FILC_OBJECT_FLAG_READONLY))
        return FR_READONLY;
    return FR_OK;
}

int fr_check(filc_ptr ptr, size_t size, int write)
{
    return fr_check_access(ptr, size, write);
}

filc_ptr fr_allocate(filc_thread* thread, size_t size, size_t alignment)
{
    filc_object* object = alignment > 16
        ? filc_allocate_with_alignment(thread, size, alignment)
        : filc_allocate(thread, size);
    return filc_ptr_create_with_object(thread, object);
}

/* Copies at most a bounded chunk; the Rust side splits large copies and
   pollchecks in between. */
int fr_copy_in(filc_thread* thread, filc_ptr dst, const void* src, size_t size)
{
    PAS_UNUSED_PARAM(thread);
    int status = fr_check_access(dst, size, 1);
    if (status)
        return status;
    if (size)
        memcpy(filc_ptr_ptr(dst), src, size);
    return FR_OK;
}

int fr_copy_out(filc_thread* thread, filc_ptr src, void* dst, size_t size)
{
    PAS_UNUSED_PARAM(thread);
    int status = fr_check_access(src, size, 0);
    if (status)
        return status;
    if (size)
        memcpy(dst, filc_ptr_ptr(src), size);
    return FR_OK;
}

/* Copies bytes between two Fil-C objects, preserving the capabilities of any
   pointers they contain (like Fil-C's memmove). */
int fr_memmove(filc_thread* thread, filc_ptr dst, filc_ptr src, size_t size)
{
    int status = fr_check_access(dst, size, 1);
    if (status)
        return status;
    status = fr_check_access(src, size, 0);
    if (status)
        return status;
    filc_memmove(thread, dst, src, size, NULL);
    return FR_OK;
}

/* Length of the NUL-terminated string at ptr, scanning no further than its
   capability allows. */
int fr_strlen(filc_ptr ptr, size_t* out)
{
    int status = fr_check_access(ptr, 0, 0);
    if (status)
        return status;
    filc_object* object = filc_ptr_object(ptr);
    const char* p = (const char*)filc_ptr_ptr(ptr);
    const char* upper = (const char*)filc_object_upper(object);
    const char* nul = upper > p ? memchr(p, 0, (size_t)(upper - p)) : NULL;
    if (!nul)
        return FR_OUT_OF_BOUNDS;
    *out = (size_t)(nul - p);
    return FR_OK;
}

int fr_load_ptr(filc_thread* thread, filc_ptr base, filc_ptr* out)
{
    *out = filc_ptr_forge_null();
    int status = fr_check_access(base, sizeof(void*), 0);
    if (status)
        return status;
    if ((uintptr_t)filc_ptr_ptr(base) % sizeof(void*))
        return FR_MISALIGNED;
    *out = filc_load_ptr(thread, base, 0);
    return FR_OK;
}

int fr_store_ptr(filc_thread* thread, filc_ptr base, filc_ptr value)
{
    int status = fr_check_access(base, sizeof(void*), 1);
    if (status)
        return status;
    if ((uintptr_t)filc_ptr_ptr(base) % sizeof(void*))
        return FR_MISALIGNED;
    filc_store_ptr(thread, base, 0, value);
    return FR_OK;
}

void fr_track(filc_thread* thread, filc_ptr ptr)
{
    filc_thread_track_object(thread, filc_ptr_object(ptr));
}

/* ---- Calls ----------------------------------------------------------------- */

/* Calls a pizlonated getter (the `pizlonated_<name>` symbol of a Fil-C
   library) and tracks the result. */
filc_ptr fr_call_getter(filc_thread* thread, void* getter)
{
    filc_ptr result = ((pizlonated_getter)getter)(thread, NULL);
    filc_thread_track_object(thread, filc_ptr_object(result));
    return result;
}

int fr_check_function(filc_ptr function)
{
    filc_object* object = filc_ptr_object(function);
    if (!object)
        return FR_NULL_CAPABILITY;
    if (filc_object_special_type(object) != FILC_SPECIAL_TYPE_FUNCTION)
        return FR_NOT_A_FUNCTION;
    if (filc_ptr_ptr(function) != filc_object_special_payload_with_manual_tracking(object))
        return FR_NOT_A_FUNCTION;
    return FR_OK;
}

/* Zeroes the first `size` bytes of the calling-convention buffers (and makes
   sure they exist). Used both to prepare call arguments and callback returns. */
void fr_cc_begin(filc_thread* thread, size_t size)
{
    filc_thread_ensure_zero_cc_total_buffer(thread, size);
}

static int fr_slot_ok(filc_thread* thread, size_t size, size_t offset)
{
    return !(offset % 8)
        && offset + 8 >= offset
        && offset + 8 <= size
        && size <= filc_thread_cc_total_size(thread);
}

int fr_cc_set_word(filc_thread* thread, size_t size, size_t offset, uint64_t value)
{
    if (!fr_slot_ok(thread, size, offset))
        return FR_BAD_SLOT;
    *(uint64_t*)filc_thread_cc_slot_at_offset(thread, offset) = value;
    *(void**)filc_thread_cc_aux_slot_at_offset(thread, offset) = NULL;
    return FR_OK;
}

int fr_cc_set_ptr(filc_thread* thread, size_t size, size_t offset, filc_ptr value)
{
    if (!fr_slot_ok(thread, size, offset))
        return FR_BAD_SLOT;
    *(void**)filc_thread_cc_aux_slot_at_offset(thread, offset) = filc_ptr_lower(value);
    *(void**)filc_thread_cc_slot_at_offset(thread, offset) = filc_ptr_ptr(value);
    return FR_OK;
}

int fr_cc_get_word(filc_thread* thread, size_t size, size_t offset, uint64_t* out)
{
    *out = 0;
    if (!fr_slot_ok(thread, size, offset))
        return FR_BAD_SLOT;
    *out = *(uint64_t*)filc_thread_cc_slot_at_offset(thread, offset);
    return FR_OK;
}

int fr_cc_get_ptr(filc_thread* thread, size_t size, size_t offset, filc_ptr* out)
{
    *out = filc_ptr_forge_null();
    if (!fr_slot_ok(thread, size, offset))
        return FR_BAD_SLOT;
    filc_ptr result = filc_ptr_create_with_lower_and_ptr_and_manual_tracking(
        *(void**)filc_thread_cc_aux_slot_at_offset(thread, offset),
        *(void**)filc_thread_cc_slot_at_offset(thread, offset));
    filc_thread_track_object(thread, filc_ptr_object(result));
    *out = result;
    return FR_OK;
}

/* Calls `function` through its generic entrypoint with the `argument_size`
   bytes of arguments already placed in the cc buffers. */
int fr_call(filc_thread* thread, filc_ptr function, size_t argument_size, size_t* return_size)
{
    *return_size = 0;
    int status = fr_check_function(function);
    if (status)
        return status;
    filc_check_function_call(function);
    filc_lock_top_native_frame(thread);
    pizlonated_function entry = ((filc_function*)filc_ptr_ptr(function))->generic_entrypoint;
    pizlonated_return_value result = entry(thread, filc_ptr_lower(function), argument_size);
    filc_unlock_top_native_frame(thread);
    *return_size = result.return_size;
    return result.has_exception ? FR_EXCEPTION : FR_OK;
}

/* Creates a Fil-C function pointer (a closure) that dispatches to the Rust
   callback registered under `id`. */
filc_ptr fr_closure_new(filc_thread* thread, uint64_t id)
{
    filc_ptr result = filc_native_zclosure_new(
        thread, fr_function_object_ptr(&fr_callback_base_object),
        filc_ptr_forge_invalid((void*)(uintptr_t)id));
    filc_thread_track_object(thread, filc_ptr_object(result));
    return result;
}
