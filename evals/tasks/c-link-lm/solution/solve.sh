#!/bin/sh
set -e
sed -i 's/-o geometry geometry.c$/-o geometry geometry.c -lm/' Makefile
