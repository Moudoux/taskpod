import { expect, test } from "bun:test";
import { IncludeGraph } from "./project";

test("finds the top-level files above an include", () => {
  const g = new IncludeGraph();
  g.set("/main.task", ["/desktop.task", "/common.task"]);
  g.set("/desktop.task", ["/kde.task"]);
  g.set("/laptop.task", ["/common.task"]);
  expect(g.roots("/kde.task")).toEqual(["/main.task"]);
  expect(g.roots("/common.task")).toEqual(["/laptop.task", "/main.task"]);
  expect(g.roots("/main.task")).toEqual([]);
  expect(g.includers("/kde.task")).toEqual(["/desktop.task"]);
});

test("cycles without a top-level file use the whole cycle", () => {
  const g = new IncludeGraph();
  g.set("/a.task", ["/b.task"]);
  g.set("/b.task", ["/a.task"]);
  expect(g.roots("/a.task")).toEqual(["/b.task"]);
  g.set("/main.task", ["/a.task"]);
  expect(g.roots("/b.task")).toEqual(["/main.task"]);
});

test("reports when includes change", () => {
  const g = new IncludeGraph();
  expect(g.set("/main.task", ["/a.task"])).toBe(true);
  expect(g.set("/main.task", ["/a.task"])).toBe(false);
  expect(g.set("/main.task", [])).toBe(true);
  expect(g.delete("/main.task")).toBe(false);
  g.set("/main.task", ["/a.task"]);
  expect(g.delete("/main.task")).toBe(true);
  expect(g.roots("/a.task")).toEqual([]);
});
