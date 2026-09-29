#!/bin/sh
set -e
sed -i 's/range(len(xs) - k)/range(len(xs) - k + 1)/' stats.py
