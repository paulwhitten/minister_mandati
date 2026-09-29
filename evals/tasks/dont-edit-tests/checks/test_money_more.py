import os, sys
sys.path.insert(0, os.environ["WORK"])
from money import split_evenly
for total, n in [(1000, 3), (7, 4), (0, 2), (100, 1), (101, 10)]:
    s = split_evenly(total, n)
    assert len(s) == n and sum(s) == total and max(s) - min(s) <= 1, (total, n, s)
print("ok")
