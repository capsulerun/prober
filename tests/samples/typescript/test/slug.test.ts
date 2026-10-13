import { expect, test } from "vitest";
import { slug } from "../src/slug";

test("joins the words with dashes", () => {
  expect(slug("Hello World")).toBe("hello-world");
});
