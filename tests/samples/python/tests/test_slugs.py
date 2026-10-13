from shop.slugs import slug


def test_joins_the_words_with_dashes():
    assert slug("Hello World") == "hello-world"
