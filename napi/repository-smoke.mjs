import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const addonPath = path.resolve("target/release/libradicle.node");
const addon = require(addonPath);
const tempBase = process.platform === "darwin" ? "/private/tmp" : os.tmpdir();
const root = fs.mkdtempSync(path.join(tempBase, "lr-repo-smoke-"));
const repository = path.join(root, "source");
const home = path.join(root, "radicle-home");
let started = false;

async function call(name, ...args) {
  const result = JSON.parse(await addon[name](...args));
  if (result?.error) throw new Error(result.error);
  return result;
}

function git(...args) {
  return execFileSync("git", args, { encoding: "utf8" }).trim();
}

try {
  git("init", "-b", "main", repository);
  fs.writeFileSync(path.join(repository, "README.md"), "first revision\n");
  git("-C", repository, "add", "README.md");
  git(
    "-C",
    repository,
    "-c",
    "user.name=Smoke Test",
    "-c",
    "user.email=smoke@example.test",
    "commit",
    "-m",
    "first commit",
  );
  const first = git("-C", repository, "rev-parse", "HEAD");

  fs.writeFileSync(path.join(repository, "README.md"), "second revision\n");
  fs.writeFileSync(path.join(repository, "new.txt"), "new file\n");
  git("-C", repository, "add", "README.md", "new.txt");
  git(
    "-C",
    repository,
    "-c",
    "user.name=Smoke Test",
    "-c",
    "user.email=smoke@example.test",
    "commit",
    "-m",
    "second commit",
  );
  const second = git("-C", repository, "rev-parse", "HEAD");

  await call("start", home, "RepositorySmoke");
  started = true;
  const { rid } = await call(
    "importRepo",
    repository,
    "repository-smoke",
    "Historical repository API smoke test",
    "main",
  );

  const oldTree = await call("treeAt", rid, first, "");
  const oldReadme = await call("blobAt", rid, first, "README.md");
  const currentTree = await call("treeAt", rid, second, "");
  const stats = await call("repoStats", rid, second);
  const remotes = await call("remotes", rid);

  assert.equal(oldTree.lastCommit.id, first);
  assert.equal(oldReadme.content, "first revision\n");
  assert.equal(currentTree.lastCommit.id, second);
  assert(currentTree.entries.some((entry) => entry.path === "new.txt"));
  assert.equal(stats.commits, 2);
  assert.equal(stats.contributors, 1);
  assert.equal(stats.branches, 1);
  assert(remotes.some((remote) => remote.delegate && remote.heads.main === second));

  console.log(JSON.stringify({ rid, first, second, stats, remotes: remotes.length }));
} finally {
  try {
    if (started) await call("shutdown");
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}
