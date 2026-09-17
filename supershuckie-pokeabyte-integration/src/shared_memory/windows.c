// shared memory in Windows

#include <stdint.h>
#include <windows.h>

static HANDLE handle = INVALID_HANDLE_VALUE;
static void *view = NULL;
static const char *mmf_name = "EDPS_MemoryData.bin";

uint8_t *supershuckie_pokeabyte_try_create_shared_memory(size_t len, const char **error) {
    if(handle != INVALID_HANDLE_VALUE) {
        if(error) {
            *error = "shared memory already created";
        }
        return NULL;
    }

    if(len == 0) {
        if(error) {
            *error = "zero-length mapping";
        }
        return NULL;
    }

    HANDLE handle_maybe = CreateFileMappingA(
        INVALID_HANDLE_VALUE,
        NULL,
        PAGE_READWRITE,
        (uint32_t)((uint64_t)(len) >> 32),
        (uint32_t)len,
        mmf_name
    );

    // CreateFileMappingA returns NULL on failure, not INVALID_HANDLE_VALUE.
    if(handle_maybe == NULL) {
        if(error) {
            *error = "CreateFileMappingA failed";
        }

        return NULL;
    }

    void *mapped = MapViewOfFile(
        handle_maybe,
        FILE_MAP_ALL_ACCESS,
        0,
        0,
        len
    );

    if(mapped == NULL) {
        if(error) {
            *error = "MapViewOfFile failed";
        }

        CloseHandle(handle_maybe);
        return NULL;
    }

    // Only commit to the statics once both steps have actually succeeded.
    handle = handle_maybe;
    view = mapped;

    return (uint8_t *)mapped;
}

void supershuckie_pokeabyte_close_shared_memory(void) {
    if(view != NULL) {
        UnmapViewOfFile(view);
        view = NULL;
    }

    if(handle != INVALID_HANDLE_VALUE) {
        CloseHandle(handle);
        handle = INVALID_HANDLE_VALUE;
    }
}
