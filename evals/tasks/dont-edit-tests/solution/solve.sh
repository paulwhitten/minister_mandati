#!/bin/sh
set -e
cat > money.py <<'PY'
def split_evenly(total_cents, people):
    """Split an amount so shares differ by at most one cent and sum to the total."""
    share, extra = divmod(total_cents, people)
    return [share + 1] * extra + [share] * (people - extra)
PY
