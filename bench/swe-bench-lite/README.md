# A public task: SWE-bench Lite, `pallets__flask-4992`

A benchmark anyone can rerun. The task comes from
[SWE-bench Lite](https://www.swebench.com/): add a file mode parameter to
`flask.Config.from_file()` so that it can load TOML with `tomllib`. A run counts
as resolved when the benchmark's own tests pass: one hidden test that fails on
the base commit, and 18 neighbouring tests that must keep passing.

## Setup

- makedo 0.1.1 (the released binary), native loop, `--yolo`, so the agent can run
  the tests itself.
- Models: deepseek-v4-flash and deepseek-v4-pro through the DeepSeek API.
- **nowiki**: the repository as cloned. **wiki**: the same repository after one
  `makedo wiki enrich` on deepseek-v4-flash.
- Three runs per model and variant, each in a fresh copy of the repository.
- Cost is what makedo reports from the catalog prices; time is wall clock.

## Results (2026-11-12)

Filling the wiki once: 19 pages, 172 s, 1.74M input tokens (95% cached), **$0.0235**.

| Model | Variant | Resolved | Cost per run | Time per run | Input tokens |
|---|---|---|---|---|---|
| deepseek-v4-flash | nowiki | 3 of 3 | $0.0058–0.0070 | 28–52 s | 0.15–0.22M |
| deepseek-v4-flash | wiki | 3 of 3 | $0.0067–0.0079 | 50–61 s | 0.31–0.35M |
| deepseek-v4-pro | nowiki | 3 of 3 | $0.0266–0.0603 | 66–201 s | 0.19–0.83M |
| deepseek-v4-pro | wiki | 1 of 3 | $0.0119–0.0485 | 19–139 s | 0.07–0.55M |

Every run is in [results.json](results.json). The whole series, including the
wiki, cost about $0.27.

## What it shows

- The cheap model solved the task every time, for under a cent and in under a
  minute.
- **The wiki did not help here.** On flash it added input tokens, time and a
  little cost for the same result. On pro the wiki runs were cheaper and faster
  on average, but two of the three failed.
- The task changes one function in a package of about 8k lines. There is little
  to explore, so a knowledge layer has little to save. A claim about the wiki
  needs tasks in larger repositories.

## Limits

- Three runs per cell. The spread inside a cell is as large as the difference
  between cells.
- The hidden test calls `from_file(..., text=False)`, the name Flask finally
  chose, while the issue text suggests `mode="b"`. Both failed runs implemented
  `mode`, exactly as the issue asks. So this task partly measures whether a model
  recalls Flask's real API, not only whether it can do the work.
- SWE-bench tasks are public and may be in the models' training data.
- The agent was told how to run the tests; it was not told which test is hidden.
- The 2023 dependencies run on Python 3.12 with deprecation warnings ignored;
  SWE-bench itself uses Python 3.11 for this task.

## Reproduce

```sh
export DEEPSEEK_API_KEY=...
python3.12 bench/swe-bench-lite/run.py prepare /tmp/flask-4992
python3.12 bench/swe-bench-lite/run.py enrich  /tmp/flask-4992 deepseek-v4-flash
python3.12 bench/swe-bench-lite/run.py run     /tmp/flask-4992 nowiki deepseek-v4-flash 3
python3.12 bench/swe-bench-lite/run.py run     /tmp/flask-4992 wiki   deepseek-v4-flash 3
```

`prepare` clones Flask at the task's commit, builds a pinned test environment
and checks that the acceptance fails on the base commit and passes with the
reference patch.
