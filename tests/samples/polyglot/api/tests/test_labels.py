from api.labels import label


def test_titles_the_name():
    assert label("pear") == "Pear"
