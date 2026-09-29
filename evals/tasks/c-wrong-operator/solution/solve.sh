#!/bin/sh
set -e
sed -i 's/a\[i\] < limit/a[i] <= limit/' count.c
