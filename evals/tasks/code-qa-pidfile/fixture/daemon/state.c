#include <stdio.h>
#include <unistd.h>
#include "state.h"

static void write_line(FILE *f, long v) { fprintf(f, "%ld\n", v); }

/* Records what a restarted daemon needs to find us. */
int persist_runtime_state(const char *dir) {
    char path[256];
    snprintf(path, sizeof path, "%s/demo.pid", dir);
    FILE *f = fopen(path, "w");
    if (!f) return -1;
    write_line(f, (long)getpid());
    return fclose(f);
}

int load_config(const char *path) { (void)path; return 0; }
