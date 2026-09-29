import os, sys
sys.path.insert(0, os.environ["WORK"])
from dates import is_leap_year
for y, want in [(2024, True), (2023, False), (1900, False), (2000, True), (2100, False)]:
    assert is_leap_year(y) == want, y
print("ok")
