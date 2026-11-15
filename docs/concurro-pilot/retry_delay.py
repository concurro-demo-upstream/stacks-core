"""An isolated capped-backoff example for a disposable review pilot."""


def retry_delay(attempt: int, cap_seconds: int = 30) -> int:
    """Return 2**attempt seconds, capped at cap_seconds.

    The caller supplies a nonnegative attempt and a positive cap.

    >>> retry_delay(0)
    1
    >>> retry_delay(4)
    16
    >>> retry_delay(5)
    30
    >>> retry_delay(8, 10)
    10
    """
    return min(2 ** attempt, cap_seconds)


if __name__ == "__main__":
    import doctest

    doctest.testmod(raise_on_error=True)
