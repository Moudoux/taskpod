import { expect, test } from "bun:test";
import { parse, resolveVar, symbolAt } from "./index";

const SOURCE = `# comment
let name = "pod"
include "other.task"

template pkg {
    needs yay
    unless pacman -Q {{ args }}
    run yay -S {{ args }}
}

task yay {
    run true
}

task apps {
    desc "Apps for {{ name }}"
    needs yay   # trailing
    let local = "x"
    pkg firefox
    unless test -e x {
        run echo {{ local }}
    }
    run {
        if [ -n "\${HOME}" ]; then
            echo "}" {{ name }}
        fi
    }
    file "/etc/x" sudo from "files/x"
    file "~/y" """
        text {{ name }}
        """
}

subtask after of apps {
    run echo done
}
`;

test("indexes definitions", () => {
  const index = parse(SOURCE);
  expect(index.tasks.map((t) => t.name)).toEqual(["yay", "apps", "after"]);
  expect(index.templates.map((t) => t.name)).toEqual(["pkg"]);
  const apps = index.tasks.find((t) => t.name === "apps")!;
  expect(apps.desc).toBe("Apps for {{ name }}");
  expect(apps.needs).toEqual(["yay"]);
  expect(apps.line).toBe(14);
  expect(apps.endLine).toBe(31);
  expect(index.tasks.find((t) => t.name === "after")!.parent).toBe("apps");
  expect(index.vars.map((v) => [v.name, v.owner])).toEqual([
    ["name", undefined],
    ["local", "apps"],
  ]);
});

test("indexes references and paths", () => {
  const index = parse(SOURCE);
  const refs = index.refs.map((r) => `${r.kind}:${r.name}`);
  expect(refs).toContain("template:pkg");
  expect(refs).toContain("task:yay");
  expect(refs).toContain("task:apps");
  expect(refs.filter((r) => r === "var:name").length).toBe(3);
  expect(refs).not.toContain("template:if");
  expect(index.paths.map((p) => `${p.kind}:${p.path}`)).toEqual([
    "include:other.task",
    "target:/etc/x",
    "from:files/x",
    "target:~/y",
  ]);
});

test("tracks line context", () => {
  const index = parse(SOURCE);
  expect(index.lines[1].context).toBe("top");
  expect(index.lines[18].context).toBe("body");
  expect(index.lines[18].owner).toBe("apps");
  expect(index.lines[24].context).toBe("shell");
  expect(index.lines[29].context).toBe("string");
  expect(index.lines[33].owner).toBeUndefined();
  expect(index.lines[34].owner).toBe("after");
});

test("finds symbols and resolves variables", () => {
  const index = parse(SOURCE);
  const line = SOURCE.split("\n")[16];
  expect(symbolAt(index, 16, line.indexOf("yay") + 1)).toEqual({ name: "yay", kind: "task", definition: false });
  expect(symbolAt(index, 14, 6)).toEqual({ name: "apps", kind: "task", definition: true });
  expect(resolveVar(index, "local", 20)?.owner).toBe("apps");
  expect(resolveVar(index, "name", 20)?.owner).toBeUndefined();
  expect(resolveVar(index, "local", 12)).toBeUndefined();
});

test("continued lines are part of the statement", () => {
  const index = parse(`template pkg {
    run echo {{ args }}
}
task a {
    pkg one \\
        two three
    needs b \\
        c
}
task b {
}
task c {
}`);
  expect(index.refs.filter((r) => r.kind === "template").map((r) => r.name)).toEqual(["pkg"]);
  expect(index.tasks[0].needs).toEqual(["b", "c"]);
});

test("indexes secrets files and secret references", () => {
  const index = parse('secrets "secrets.yaml"\ntask t {\n    file "/etc/x" "{{ secrets.wifi.password }}"\n}\n');
  expect(index.paths.map((p) => `${p.kind}:${p.path}`)).toEqual(["secrets:secrets.yaml", "target:/etc/x"]);
  expect(parse('age-key "k.age"\n').paths.map((p) => `${p.kind}:${p.path}`)).toEqual(["age-key:k.age"]);
  expect(index.refs.map((r) => `${r.kind}:${r.name}`)).toContain("var:secrets.wifi.password");
});
