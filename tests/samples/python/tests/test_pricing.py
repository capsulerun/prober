from shop.pricing import total


class TestTotal:
    def test_adds_the_prices(self):
        assert total([1, 2, 3]) == 6
