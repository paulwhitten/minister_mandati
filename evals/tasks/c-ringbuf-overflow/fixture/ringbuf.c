#include "ringbuf.h"

void rb_init(struct ringbuf *rb) {
    rb->head = rb->tail = rb->count = 0;
}

int rb_push(struct ringbuf *rb, int v) {
    if (rb->count == RB_CAP) {
        return -1;
    }
    rb->data[rb->head] = v;
    rb->head++;
    if (rb->head > RB_CAP) {
        rb->head = 0;
    }
    rb->count++;
    return 0;
}

int rb_pop(struct ringbuf *rb, int *v) {
    if (rb->count == 0) {
        return -1;
    }
    *v = rb->data[rb->tail];
    rb->tail = (rb->tail + 1) % RB_CAP;
    rb->count--;
    return 0;
}
