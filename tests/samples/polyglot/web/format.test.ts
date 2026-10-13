import { expect, test } from "vitest";
import { euros } from "./format";

test("formats euros", () => {
  expect(euros(3)).toBe("3.00 €");
});
