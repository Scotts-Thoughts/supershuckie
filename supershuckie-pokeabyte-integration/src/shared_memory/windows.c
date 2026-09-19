// shared memory in Windows

#include <stdint.h>
#include <stdlib.h>
#include <windows.h>

// One mapping. Several may be open at once (one per Poke-A-Byte integration server, each under
// its own name), so nothing here is static.
struct supershuckie_pokeabyte_shm {
    HANDLE handle;
    void *view;
};

void *supershuckie_pokeabyte_try_create_shared_memory(const char *name, size_t len, const char **error, uint8_t **memory) {
    if(memory) {
        *memory = NULL;
    }

    if(name == NULL || name[0] == 0) {
        if(error) {
            *error = "no mapping name";
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
        name
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

    struct supershuckie_pokeabyte_shm *shm = malloc(sizeof(*shm));
    if(shm == NULL) {
        if(error) {
            *error = "out of memory";
        }
        UnmapViewOfFile(mapped);
        CloseHandle(handle_maybe);
        return NULL;
    }

    shm->handle = handle_maybe;
    shm->view = mapped;

    if(memory) {
        *memory = (uint8_t *)mapped;
    }

    return shm;
}

void supershuckie_pokeabyte_close_shared_memory(void *token) {
    struct supershuckie_pokeabyte_shm *shm = token;
    if(shm == NULL) {
        return;
    }

    if(shm->view != NULL) {
        UnmapViewOfFile(shm->view);
    }

    if(shm->handle != NULL && shm->handle != INVALID_HANDLE_VALUE) {
        CloseHandle(shm->handle);
    }

    free(shm);
}
