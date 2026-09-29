#!/bin/sh
set -e
git checkout -q -b fix/typo
sed -i 's/teh plan/the plan/' notes.txt
git commit -q -am 'Fix typo in notes'
