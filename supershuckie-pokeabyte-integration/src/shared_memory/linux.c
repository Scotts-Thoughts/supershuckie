// shared memory in linux
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
    char *path;
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

    // Poke-A-Byte opens the file by this fixed path (its SharedConstants.GetMmfPath()).
    static const char *prefix = "/dev/shm/";
    char *path = malloc(strlen(prefix) + strlen(name) + 1);
    if(path == NULL) {
        if(error) {
            *error = "out of memory";
        }
        return NULL;
    }
    strcpy(path, prefix);
    strcat(path, name);

    int new_fd = open(path, O_CREAT|O_RDWR, 0644);
    if(new_fd < 0) {
        if(error) {
            *error = "open failed";
        }
        free(path);
        return NULL;
    }

    if(ftruncate(new_fd, len) != 0) {
        if(error) {
            *error = "ftruncate failed";
        }
        close(new_fd);
        free(path);
        return NULL;
    }

    uint8_t *f = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, new_fd, 0);

    if(f == (void *)-1) {
        if(error) {
            *error = "mmap failed";
        }
        close(new_fd);
        free(path);
        return NULL;
    }

    struct supershuckie_pokeabyte_shm *shm = malloc(sizeof(*shm));
    if(shm == NULL) {
        if(error) {
            *error = "out of memory";
        }
        munmap(f, len);
        close(new_fd);
        free(path);
        return NULL;
    }

    shm->fd = new_fd;
    shm->mapped = f;
    shm->mapped_len = len;
    shm->path = path;

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
    free(shm->path);
    free(shm);
}
