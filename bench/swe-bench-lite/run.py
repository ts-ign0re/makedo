#!/usr/bin/env python3
"""Run one SWE-bench Lite task through makedo and score it with the benchmark's own tests.

    run.py prepare <workdir>                      fetch the task, clone the repo, build the test env, validate it
    run.py enrich  <workdir> <model>              fill the project wiki once (the "wiki" variant starts from it)
    run.py run     <workdir> <variant> <model> <n>   variant: nowiki | wiki
    run.py rescore <workdir>                      re-score finished runs without calling the model

Needs DEEPSEEK_API_KEY in the environment and `makedo` on PATH (or MAKEDO_BIN).
Results are appended to <workdir>/results.json.

ponytail: one task with its dependency pins hardcoded below; generalise when a second task is added.
"""
import json, os, re, shutil, subprocess, sys, time, urllib.parse, urllib.request
from pathlib import Path

INSTANCE = "pallets__flask-4992"
REPO = "https://github.com/pallets/flask"
# what Flask 2.3-dev resolved to in early 2023; the hidden test needs tomllib, so Python >= 3.11
PINS = ["werkzeug==2.2.3", "jinja2==3.1.2", "itsdangerous==2.1.2", "click==8.1.3", "markupsafe==2.1.2", "pytest==7.2.2"]
TEST_FILE = "tests/test_config.py"
MAKEDO = os.environ.get("MAKEDO_BIN", "makedo")
TIMEOUT = 20 * 60


def sh(cmd, cwd=None, check=True, **kw):
    return subprocess.run(cmd, cwd=cwd, check=check, text=True, capture_output=True, **kw)


def tests(work, repo):
    """(passed, failed) for the task's test file in the pinned environment."""
    out = sh([str(work / "runtests"), TEST_FILE], cwd=repo, check=False).stdout
    m = re.search(r"(?:(\d+) failed, )?(\d+) passed", out)
    return (int(m.group(2)), int(m.group(1) or 0)) if m else (0, -1)


def prepare(work):
    work.mkdir(parents=True, exist_ok=True)
    where = urllib.parse.quote(f"\"instance_id\"='{INSTANCE}'")
    url = f"https://datasets-server.huggingface.co/filter?dataset=princeton-nlp%2FSWE-bench_Lite&config=default&split=test&where={where}&length=1"
    cache = work / "instance.json"  # the dataset server answers 500 now and then; fetch once
    if not cache.exists():
        cache.write_text(json.dumps(json.load(urllib.request.urlopen(url))["rows"][0]["row"]))
    row = json.loads(cache.read_text())
    (work / "problem.txt").write_text(row["problem_statement"])
    (work / "gold.patch").write_text(row["patch"])
    (work / "test.patch").write_text(row["test_patch"])
    repo = work / "template"
    if not repo.exists():
        sh(["git", "clone", "-q", REPO, str(repo)])
        sh(["git", "checkout", "-q", row["base_commit"]], cwd=repo)
    if not (work / "venv").exists():
        assert sys.version_info >= (3, 11), "run this script with Python 3.11+: the hidden test needs tomllib"
        sh([sys.executable, "-m", "venv", str(work / "venv")])
        sh([str(work / "venv/bin/pip"), "install", "-q", *PINS])
    # DeprecationWarnings come from running 2023 dependencies on a newer Python; the repo treats warnings as errors
    (work / "runtests").write_text(
        f'#!/bin/sh\nPYTHONPATH=src exec "{work}/venv/bin/python" -m pytest -W ignore::DeprecationWarning -q "$@"\n')
    (work / "runtests").chmod(0o755)
    # the acceptance must fail on the base commit and pass with the reference patch
    sh(["git", "apply", str(work / "test.patch")], cwd=repo)
    before = tests(work, repo)
    sh(["git", "apply", str(work / "gold.patch")], cwd=repo)
    after = tests(work, repo)
    sh(["git", "stash", "-q", "-u"], cwd=repo)
    sh(["git", "stash", "drop", "-q"], cwd=repo)
    assert before[1] == 1 and after[1] == 0, f"acceptance is not valid: base {before}, reference {after}"
    print(f"ready: base {before[0]} passed / {before[1]} failed, reference {after[0]} passed / {after[1]} failed")


def score(work, repo):
    """Score like SWE-bench: the benchmark's tests replace whatever the agent did under tests/,
    including files it created there (the test patch adds files and would not apply otherwise)."""
    sh(["git", "checkout", "-q", "HEAD", "--", "tests"], cwd=repo)
    sh(["git", "clean", "-fdq", "--", "tests"], cwd=repo)
    applied = sh(["git", "apply", str(work / "test.patch")], cwd=repo, check=False).returncode == 0
    passed, failed = tests(work, repo) if applied else (0, -1)
    return {"passed": passed, "failed": failed, "resolved": applied and failed == 0}


def rescore(work):
    """Recompute the scores of finished runs from their working copies, without calling the model."""
    results = work / "results.json"
    records = json.loads(results.read_text())
    for rec in records:
        rec.update(score(work, work / "runs" / f"{rec['variant']}-{rec['model']}-{rec['run']}"))
    results.write_text(json.dumps(records, indent=2))


def makedo(work, repo, model, args):
    """Run makedo in `repo`; returns usage parsed from its `[usage]` line plus wall time."""
    home = work / f"home-{model}"
    home.mkdir(exist_ok=True)
    (home / "config.toml").write_text(f'[provider]\nkind = "deepseek"\nmodel = "{model}"\n')
    start = time.time()
    try:
        p = sh([MAKEDO, *args], cwd=repo, check=False, timeout=TIMEOUT, env={**os.environ, "MAKEDO_HOME": str(home)})
        log = p.stdout + p.stderr
    except subprocess.TimeoutExpired as e:
        log = (e.stdout or "") + (e.stderr or "") if isinstance(e.stdout, str) else "timeout"
    usage = {"seconds": round(time.time() - start)}
    m = re.search(r"\[usage\] in=(\d+) out=(\d+) cache_read=(\d+).*?cost=\$([\d.]+)", log)
    if m:
        usage.update({"in": int(m[1]), "out": int(m[2]), "cache_read": int(m[3]), "cost": float(m[4])})
    return usage, log


def enrich(work, model):
    repo = work / "wiki-template"
    shutil.rmtree(repo, ignore_errors=True)
    shutil.copytree(work / "template", repo, symlinks=True)
    usage, log = makedo(work, repo, model, ["wiki", "enrich"])
    nodes = len(list((repo / ".makedo/wiki").glob("*.md")))
    (work / "enrich.json").write_text(json.dumps({"model": model, "wiki_files": nodes, **usage}, indent=2))
    (work / "enrich.log").write_text(log)
    print(json.dumps({"wiki_files": nodes, **usage}))


def run(work, variant, model, n):
    problem = (work / "problem.txt").read_text()
    prompt = (f"{problem}\n\nImplement this in the repository in the current directory. "
              f"Before you finish, run the tests with `{work}/runtests {TEST_FILE}` and make sure they pass.")
    results = work / "results.json"
    records = json.loads(results.read_text()) if results.exists() else []
    for i in range(1, n + 1):
        repo = work / "runs" / f"{variant}-{model}-{i}"
        shutil.rmtree(repo, ignore_errors=True)
        repo.parent.mkdir(exist_ok=True)
        shutil.copytree(work / ("wiki-template" if variant == "wiki" else "template"), repo, symlinks=True)
        usage, log = makedo(work, repo, model, ["-p", prompt, "--yolo"])
        (repo.parent / f"{repo.name}.log").write_text(log)
        stat = sh(["git", "diff", "--numstat", "--", "src"], cwd=repo).stdout.split("\n")
        diff = sum(int(a) + int(b) for a, b, *_ in (l.split("\t") for l in stat if l.strip()) if a.isdigit())
        rec = {"variant": variant, "model": model, "run": i, **usage, "diff_lines": diff, **score(work, repo)}
        records.append(rec)
        results.write_text(json.dumps(records, indent=2))
        print(json.dumps(rec))


if __name__ == "__main__":
    cmd, work = sys.argv[1], Path(sys.argv[2]).resolve()
    if cmd == "prepare":
        prepare(work)
    elif cmd == "enrich":
        enrich(work, sys.argv[3])
    elif cmd == "rescore":
        rescore(work)
    elif cmd == "run":
        run(work, sys.argv[3], sys.argv[4], int(sys.argv[5]))
    else:
        sys.exit(__doc__)
