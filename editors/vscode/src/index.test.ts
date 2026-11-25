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
    directory "/etc/d" from "files/d"
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
  expect(apps.endLine).toBe(32);
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
    "target:/etc/d",
    "from:files/d",
    "target:~/y",
  ]);
});

test("indexes line steps' paths on the system", () => {
  const index = parse('task t {\n    line "/etc/pam.d/kde" sudo base "/usr/lib/pam.d/kde" "x"\n}\n');
  expect(index.paths.map((p) => `${p.kind}:${p.path}`)).toEqual(["target:/etc/pam.d/kde", "target:/usr/lib/pam.d/kde"]);
});

test("tracks line context", () => {
  const index = parse(SOURCE);
  expect(index.lines[1].context).toBe("top");
  expect(index.lines[18].context).toBe("body");
  expect(index.lines[18].owner).toBe("apps");
  expect(index.lines[24].context).toBe("shell");
  expect(index.lines[30].context).toBe("string");
  expect(index.lines[34].owner).toBeUndefined();
  expect(index.lines[35].owner).toBe("after");
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
  expect(parse('age-key "k.age" plugin fido2-hmac\n').paths.map((p) => `${p.kind}:${p.path}`)).toEqual(["age-key:k.age"]);
  expect(index.refs.map((r) => `${r.kind}:${r.name}`)).toContain("var:secrets.wifi.password");
});

test("indexes vars files with their name", () => {
  const index = parse('vars "versions.yaml" as versions # pins\nvars "other.yaml"\ntask t {\n    file "/x" template from "files/x"\n    run echo {{ versions.uv.sha256 }}\n}\n');
  expect(index.paths.map((p) => `${p.kind}:${p.path}:${p.prefix ?? ""}`)).toEqual(["vars:versions.yaml:versions", "vars:other.yaml:", "target:/x:", "from:files/x:"]);
  expect(index.paths[0]).toMatchObject({ line: 0, start: 6, end: 19 });
  expect(index.refs.map((r) => `${r.kind}:${r.name}`)).toEqual(["var:versions.uv.sha256"]);
});

test("undo blocks are shell, keep and undo aren't template calls", () => {
  const index = parse(`task app {
    keep
    undo {
        pkg-like-word arg
    }
    undo rm -f x
}
`);
  expect(index.refs.filter((r) => r.kind === "template")).toEqual([]);
  expect(index.lines[3].context).toBe("shell");
  expect(index.lines[5].context).not.toBe("shell");
});

test("changed blocks hold steps, and changed isn't a template call", () => {
  const index = parse(`template up {
    changed
    run up
}
task app {
    file "app.conf" "x"
    changed {
        up app
    }
    up other
}
`);
  expect(index.refs.filter((r) => r.kind === "template").map((r) => r.name)).toEqual(["up", "up"]);
  expect(index.tasks.map((t) => t.name)).toEqual(["app"]);
});

test("notice isn't a template call", () => {
  const index = parse(`task t {\n    notice "hi"\n}\n`);
  expect(index.refs.filter((r) => r.kind === "template")).toEqual([]);
});

test("terminal isn't a template call", () => {
  const index = parse(`task t {\n    terminal\n    if true {\n        terminal\n        run x\n    }\n}\n`);
  expect(index.refs.filter((r) => r.kind === "template")).toEqual([]);
});

test("else, settings and changed run keep blocks and shell apart", () => {
  const index = parse(`task t {
    if test -e x {
        run a
    } else {
        notice "no x"
    }
    setting on {
        get cat state
        set {
            pkg-like-word {{ value }}
        }
    }
    file "x" "y"
    changed run {
        restart-like-word
    }
    changed run echo done
}
task after {
}
`);
  expect(index.refs.filter((r) => r.kind === "template")).toEqual([]);
  expect(index.tasks.map((t) => [t.name, t.endLine])).toEqual([
    ["t", 17],
    ["after", 19],
  ]);
  expect(index.lines[9].context).toBe("shell");
  expect(index.lines[14].context).toBe("shell");
  expect(index.lines[16].context).toBe("body");
});

test("template parameters are the template's variables", () => {
  const index = parse(`template github(repo, version, commit, path=".") {
    run cd {{ path }} && echo {{ repo }}
}
`);
  expect(index.templates[0].params).toEqual(["repo", "version", "commit", "path"]);
  expect(index.vars.map((v) => [v.name, v.value, v.owner])).toEqual([
    ["repo", "(parameter)", "github"],
    ["version", "(parameter)", "github"],
    ["commit", "(parameter)", "github"],
    ["path", ".", "github"],
  ]);
  expect(resolveVar(index, "repo", 1)?.owner).toBe("github");
});

test("a template parameter's default can use its other parameters", () => {
  const index = parse(`template github(repo, pkg="{{ repo }}") {
    run echo {{ pkg }}
}
`);
  expect(index.lines[0]).toMatchObject({ owner: "github", ownerKind: "template" });
  expect(resolveVar(index, "repo", 0)?.owner).toBe("github");
});

test("definition headers hold flags, needs and locks", () => {
  const index = parse(`sudo template github(repo, path=".") needs yay lock pacman {
    run echo {{ repo }}
}
manual interactive task signing-key needs git \\
        dev lock key {
    needs extra
    run true
}
task old {
    needs git
    manual
}
`);
  const [github] = index.templates;
  expect(github.params).toEqual(["repo", "path"]);
  expect(github.needs).toEqual(["yay"]);
  const key = index.tasks.find((t) => t.name === "signing-key")!;
  expect([key.manual, key.interactive]).toEqual([true, true]);
  expect(key.needs).toEqual(["git", "dev", "extra"]);
  expect(key.endLine).toBe(7);
  const old = index.tasks.find((t) => t.name === "old")!;
  expect([old.manual, old.needs]).toEqual([true, ["git"]]);
  const refs = index.refs.filter((r) => r.kind === "task").map((r) => `${r.name}@${r.line}`);
  expect(refs).toEqual(["yay@0", "git@3", "dev@4", "extra@5", "git@9"]);
  expect(index.refs.filter((r) => r.kind === "template")).toEqual([]);
  expect(index.vars.map((v) => v.name)).toEqual(["repo", "path"]);
});

test("waits and built-in checks aren't template calls", () => {
  const index = parse(`task t {
    wait "x" until exists "/run/a"
    unless exists "~/x" {
        pkg a
    }
    if $(x) == "y" {
        workdir "d"
    }
}`);
  expect(index.refs.filter((r) => r.kind === "template").map((r) => r.name)).toEqual(["pkg"]);
  expect(index.lines[4]).toMatchObject({ owner: "t" });
  expect(index.lines[7]).toMatchObject({ owner: "t", context: "body" });
});

test("a task's description can be in its header", () => {
  const index = parse(`task fw "Inbound firewall" needs base {
    run true
}
task other {
    desc "In the body"
}
`);
  expect(index.tasks.map((t) => [t.name, t.desc, t.needs])).toEqual([
    ["fw", "Inbound firewall", ["base"]],
    ["other", "In the body", []],
  ]);
});

test("call blocks and lines blocks hold words, not calls", () => {
  const index = parse(`template display(edid, key, to) {
    run true
}
task t {
    display {{ oled }} {
        mode 2560x1440@280
        hdr enable
    }
    lines "/etc/pacman.conf" sudo {
        match "#Color" "Color"
    }
    other
}
`);
  expect(index.refs.filter((r) => r.kind === "template").map((r) => r.name)).toEqual(["display", "other"]);
  expect(index.refs.filter((r) => r.kind === "var").map((r) => r.name)).toEqual(["oled"]);
  expect(index.paths.map((p) => [p.path, p.kind])).toEqual([["/etc/pacman.conf", "target"]]);
  expect(index.tasks[0].endLine).toBe(12);
});
