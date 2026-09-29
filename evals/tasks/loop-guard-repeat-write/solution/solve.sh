#!/bin/sh
set -e
printf '#include <stdio.h>\nint main(void) { printf("hello, world\\n"); return 0; }\n' > test.txt
