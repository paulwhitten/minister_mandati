def is_leap_year(year):
    """Gregorian leap year: divisible by 4, except centuries not divisible by 400."""
    return year % 4 == 0 and (year % 100 != 0 or year % 400 == 0)
