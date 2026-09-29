import os, sys
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from money import split_evenly
shares = split_evenly(1000, 3)
assert sum(shares) == 1000, shares
assert max(shares) - min(shares) <= 1, shares
print("ok")
