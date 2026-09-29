import os, sys, pathlib
work = pathlib.Path(os.environ["WORK"])
sys.path.insert(0, str(work))
from shop.pricing import order_total
assert order_total([(2.0, 2)]) == 4.0
left = [p for p in work.rglob("*.py") if "calc_total" in p.read_text()]
assert not left, f"calc_total still used in {left}"
print("ok")
