#!/bin/sh
set -e
{ echo status,count; awk '{print $(NF-1)}' access.log | sort -n | uniq -c | awk '{print $2","$1}'; } > summary.csv
