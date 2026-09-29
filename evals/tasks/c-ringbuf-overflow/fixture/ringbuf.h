#ifndef RINGBUF_H
#define RINGBUF_H
#define RB_CAP 4
struct ringbuf {
    int data[RB_CAP];
    int head, tail, count;
};
void rb_init(struct ringbuf *rb);
int rb_push(struct ringbuf *rb, int v); /* 0 on success, -1 when full */
int rb_pop(struct ringbuf *rb, int *v); /* 0 on success, -1 when empty */
#endif
