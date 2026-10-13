import { expect, test } from "vitest";
import prices from "../data/prices.json";

test("an apple costs 3", () => {
  expect(prices.apple).toBe(3);
});
