#!/bin/sh
set -e
cd data && find . -type f -printf '%s %P\n' | sort -rn | head -3 | cut -d' ' -f2 > ../report.txt
