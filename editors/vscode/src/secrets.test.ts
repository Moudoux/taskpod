import { expect, test } from "bun:test";
import { secretKeys } from "./secrets";

const names = (text: string, file: string) => secretKeys(text, file).map((k) => `${k.name}${k.leaf ? "" : "/"}@${k.line}:${k.start}`);

test("yaml: nested keys, lists and sops metadata", () => {
  const yaml = `# comment
wifi:
    password: ENC[AES256_GCM,data:abc=,iv:x,tag:y,type:str]
    "ssid": ENC[AES256_GCM,data:def,type:str]
tokens:
- ENC[AES256_GCM,data:1,type:str]
- name: ENC[AES256_GCM,data:2,type:str]
  value: ENC[AES256_GCM,data:3,type:str]
cert: |
    not: a key
top: ENC[AES256_GCM,data:4,type:str]
sops:
    age:
        - recipient: age1xyz
    mac: ENC[...]
`;
  expect(names(yaml, "secrets.yaml")).toEqual([
    "wifi/@1:0",
    "wifi.password@2:4",
    "wifi.ssid@3:4",
    "tokens/@4:0",
    "tokens.0@5:0",
    "tokens.1.name@6:2",
    "tokens.1.value@7:2",
    "cert@8:0",
    "top@10:0",
  ]);
});

test("yaml: indented lists", () => {
  expect(names("a:\n  - x\n  - y\nb: z\n", "s.yml")).toEqual(["a/@0:0", "a.0@1:2", "a.1@2:2", "b@3:0"]);
});

test("json", () => {
  const json = `{
\t"wifi": {"password": "ENC[x]", "list": ["ENC[a]", {"k": "ENC[b]"}]},
\t"a\\"b": "ENC[c]",
\t"sops": {"mac": "ENC[m]"}
}`;
  expect(names(json, "s.json")).toEqual([
    "wifi/@1:1",
    "wifi.password@1:10",
    "wifi.list/@1:32",
    "wifi.list.0@1:41",
    "wifi.list.1/@1:51",
    "wifi.list.1.k@1:52",
    'a"b@2:1',
  ]);
});

test("dotenv and ini", () => {
  expect(names("A=ENC[x]\nexport B = ENC[y]\nsops_mac=ENC[m]\n# C=no\n", "secrets.env")).toEqual(["A@0:0", "B@1:7"]);
  expect(names("[db]\npass = ENC[x]\n[sops]\nmac = m\n", "s.ini")).toEqual(["db/@0:1", "db.pass@1:0"]);
});
