#!/bin/sh
set -e
sed -i 's/return x \* 7 - 207;/return x * 7 + 207;/' table.c
