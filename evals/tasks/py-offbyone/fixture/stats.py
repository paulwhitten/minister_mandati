"""Small statistics helpers."""


def mean(xs):
    return sum(xs) / len(xs)


def moving_sum(xs, k):
    """Sums of every window of k consecutive items."""
    return [sum(xs[i:i + k]) for i in range(len(xs) - k)]


def spread(xs):
    return max(xs) - min(xs)
