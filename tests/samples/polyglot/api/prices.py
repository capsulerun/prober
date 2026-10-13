import json

with open("data/prices.json") as prices_file:
    PRICES = json.load(prices_file)
