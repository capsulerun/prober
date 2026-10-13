import { expect, test } from "vitest";
import { total } from "../src/cart";

test("adds the prices", () => {
  expect(total([1, 2, 3])).toBe(6);
});
