import os, sys
sys.path.insert(0, os.environ["WORK"])
from stats import moving_sum, mean, spread
assert moving_sum([1, 2, 3, 4], 2) == [3, 5, 7]
assert moving_sum([5], 1) == [5]
assert moving_sum([1, 2, 3], 3) == [6]
assert moving_sum([1, 2], 3) == []
assert mean([2, 4]) == 3 and spread([1, 9, 4]) == 8
print("ok")
