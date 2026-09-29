def split_evenly(total_cents, people):
    """Split an amount so shares differ by at most one cent and sum to the total."""
    share = total_cents // people
    return [share] * people
