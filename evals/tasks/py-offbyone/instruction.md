`moving_sum` in `stats.py` drops the last window: for `[1, 2, 3, 4]` with `k=2`
it returns `[3, 5]` instead of `[3, 5, 7]`. Fix the function.
