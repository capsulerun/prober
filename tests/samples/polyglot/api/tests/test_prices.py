from api.prices import PRICES


def test_an_apple_costs_3():
    assert PRICES["apple"] == 3
