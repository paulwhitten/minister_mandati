#include <stdio.h>
#include "ringbuf.h"
int main(void) {
    struct ringbuf rb; int v;
    rb_init(&rb);
    for (int round = 0; round < 5; round++) {
        for (int i = 0; i < RB_CAP; i++) if (rb_push(&rb, round * 10 + i)) { puts("push failed"); return 1; }
        if (rb_push(&rb, 99) != -1) { puts("overfull push accepted"); return 1; }
        for (int i = 0; i < RB_CAP; i++) {
            if (rb_pop(&rb, &v) || v != round * 10 + i) { printf("bad pop %d\n", v); return 1; }
        }
        rb_push(&rb, 1); rb_pop(&rb, &v); /* shift the start */
    }
    puts("ok"); return 0;
}
