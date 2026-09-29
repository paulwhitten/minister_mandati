def calc_total(items):
    """Sum of price * quantity."""
    return sum(price * qty for price, qty in items)
