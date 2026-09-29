#!/bin/sh
set -e
sed -i 's/if (rb->head > RB_CAP) {/if (rb->head >= RB_CAP) {/' ringbuf.c
