// shared memory in macOS

#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <string.h>
#include <stdbool.h>

// One mapping. Several may be open at once (one per Poke-A-Byte integration server, each under
// its own name), so nothing here is static.
struct supershuckie_pokeabyte_shm {
    int fd;
    uint8_t *mapped;
    size_t mapped_len;
    char *shm_name;
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

    // Poke-A-Byte shm_open()s this exact name (its SharedConstants.GetMmfPath()); it is not a
    // file on disk.
    static const char *prefix = "/tmp/";
    char *shm_name = malloc(strlen(prefix) + strlen(name) + 1);
    if(shm_name == NULL) {
        if(error) {
            *error = "out of memory";
        }
        return NULL;
    }
    strcpy(shm_name, prefix);
    strcat(shm_name, name);

    // Remove the shared memory if it already exists; ftruncate only works once per shared memory.
    shm_unlink(shm_name);

    int new_fd = shm_open(shm_name, O_CREAT|O_RDWR, S_IRUSR|S_IWUSR);
    if(new_fd < 0) {
        if(error) {
            *error = "shm_open failed";
        }
        free(shm_name);
        return NULL;
    }

    if(ftruncate(new_fd, len) != 0) {
        if(error) {
            *error = "ftruncate failed";
        }
        close(new_fd);
        free(shm_name);
        return NULL;
    }

    uint8_t *f = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, new_fd, 0);

    if(f == (void *)-1) {
        if(error) {
            *error = "mmap failed";
        }
        close(new_fd);
        free(shm_name);
        return NULL;
    }

    struct supershuckie_pokeabyte_shm *shm = malloc(sizeof(*shm));
    if(shm == NULL) {
        if(error) {
            *error = "out of memory";
        }
        munmap(f, len);
        close(new_fd);
        free(shm_name);
        return NULL;
    }

    shm->fd = new_fd;
    shm->mapped = f;
    shm->mapped_len = len;
    shm->shm_name = shm_name;

    if(error) {
        *error = "succeeded";
    }

    if(memory) {
        *memory = f;
    }

    return shm;
}

void supershuckie_pokeabyte_close_shared_memory(void *token) {
    struct supershuckie_pokeabyte_shm *shm = token;
    if(shm == NULL) {
        return;
    }

    if(shm->mapped != NULL) {
        munmap(shm->mapped, shm->mapped_len);
    }

    close(shm->fd);
    free(shm->shm_name);
    free(shm);
}
