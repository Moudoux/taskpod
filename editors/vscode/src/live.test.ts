import { expect, test } from "bun:test";
import { type TaskResult, changeSummary, hoverMarkdown, parseResult, report, statusSummary, tasksIn, title } from "./live";

// Output of `taskpod main.task -D --json --no-secrets`, trimmed.
const OUTPUT = JSON.stringify({
  version: 1,
  file: "/cfg/main.task",
  mode: "diff",
  pull: false,
  skipped: null,
  error: null,
  tasks: [
    { name: "same", file: "/cfg/main.task", line: 3, outcome: "up-to-date", reason: null, unchecked: 0, steps: [{ depth: 0, kind: "ok", label: "file /etc/a" }] },
    {
      name: "pacman",
      file: "/cfg/system.task",
      line: 12,
      outcome: "would-change",
      reason: null,
      unchecked: 1,
      steps: [
        { depth: 0, kind: "change", label: "file /etc/pacman.conf · content", diff: ["@@ -1 +1 @@", "-#Color", "+Color"] },
        { depth: 0, kind: "change", label: "aur paru" },
        { depth: 1, kind: "run", label: "makepkg -si" },
        { depth: 0, kind: "run", label: "pacman -Sy" },
        { depth: 0, kind: "unchecked", label: "file ~/.netrc" },
      ],
    },
    { name: "guarded", file: "/cfg/main.task", line: 9, outcome: "skipped", reason: "unless pacman -Q foo", unchecked: 0, steps: [] },
    { name: "broken", file: "/cfg/main.task", line: 20, outcome: "failed", reason: "file /x: failed to read /y: No such file or directory (os error 2)", unchecked: 0, steps: [] },
    { name: "after", file: "/cfg/main.task", line: 25, outcome: "blocked", reason: "broken", unchecked: 0, steps: [] },
    { name: "secret", file: "/cfg/main.task", line: 30, outcome: "unchecked", reason: "secret 'token': not checked", unchecked: 0, steps: [] },
  ],
});

const result = parseResult(OUTPUT);
const task = (name: string) => result.tasks.find((t) => t.name === name) as TaskResult;

test("parses and rejects other versions", () => {
  expect(result.tasks.length).toBe(6);
  expect(() => parseResult(JSON.stringify({ version: 2, tasks: [] }))).toThrow("version 2");
  expect(() => parseResult("not json")).toThrow();
  expect(() => parseResult("{}")).toThrow("not a taskpod");
});

test("groups tasks by the file that defines them", () => {
  expect([...tasksIn([result], "/cfg/main.task").keys()]).toEqual(["same", "guarded", "broken", "after", "secret"]);
  expect([...tasksIn([result], "/cfg/system.task").keys()]).toEqual(["pacman"]);
  expect(tasksIn([result], "/other.task").size).toBe(0);
});

test("titles", () => {
  expect(title(task("same"))).toBe("✓ up to date");
  expect(changeSummary(task("pacman"))).toBe("1 file, 1 block, 1 command");
  expect(title(task("pacman"))).toBe("~ 1 file, 1 block, 1 command · 1 not checked");
  expect(title(task("guarded"))).toBe("⊘ skipped: unless pacman -Q foo");
  expect(title(task("broken"))).toBe("✗ failed: file /x: failed to read /y: No such file or directory (os e…");
  expect(title(task("after"))).toBe("✗ not checked, broken failed");
  expect(title(task("secret"))).toBe("? not checked: needs secrets");
});

test("hover shows steps and diffs", () => {
  const markdown = hoverMarkdown(task("pacman"));
  expect(markdown).toContain("**~ 1 file, 1 block, 1 command · 1 not checked**");
  expect(markdown).toContain("→ aur paru\n  $ makepkg -si");
  expect(markdown).toContain("? file ~/.netrc · uses secrets, not checked");
  expect(markdown).toContain("```diff\n@@ -1 +1 @@\n-#Color\n+Color\n```");
  // A long reason is shown in full.
  expect(hoverMarkdown(task("broken"))).toContain("(os error 2)");
  // Diff text with backticks can't close the fence early.
  const tricky: TaskResult = { ...task("same"), outcome: "would-change", steps: [{ depth: 0, kind: "change", label: "file x", diff: ["+```"] }] };
  expect(hoverMarkdown(tricky)).toContain("````diff\n+```\n````");
});

test("report and status", () => {
  expect(report(task("pacman"))).toContain("→ file /etc/pacman.conf · content\n  @@ -1 +1 @@\n  -#Color");
  expect(statusSummary(result.tasks)).toBe("1 would change, 2 failed, 1 not checked");
  expect(statusSummary([task("same")])).toBe("up to date");
  expect(statusSummary([])).toBe("nothing to check");
});
