import os, sys
sys.path.insert(0, os.environ["WORK"])
from grades import summarize
got = summarize({"math": {"ann": [80, 90], "bo": []}, "art": {"cy": [70]}})
assert got == {"math": {"ann": 85.0}, "art": {"cy": 70.0}}, got
print("ok")
