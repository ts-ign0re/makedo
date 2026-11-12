# Benchmarks

The project's own measurements. The first section uses a public task and can be
rerun by anyone. The numbered sections were measured on private repositories, so
their task code, patches and transcripts are not published — only the numbers.
Sample sizes are small throughout: read these as directions, not statistics.

## A public task anyone can rerun

One task from SWE-bench Lite, `pallets__flask-4992`: add a file mode parameter to
`flask.Config.from_file()`. makedo 0.1.1 on DeepSeek, three runs per cell, scored
by the benchmark's own tests. The script, every run and the full list of limits
are in [bench/swe-bench-lite](../bench/swe-bench-lite).

| Model | Variant | Resolved | Cost per run | Time per run |
|---|---|---|---|---|
| deepseek-v4-flash | no wiki | 3 of 3 | $0.0058–0.0070 | 28–52 s |
| deepseek-v4-flash | wiki | 3 of 3 | $0.0067–0.0079 | 50–61 s |
| deepseek-v4-pro | no wiki | 3 of 3 | $0.0266–0.0603 | 66–201 s |
| deepseek-v4-pro | wiki | 1 of 3 | $0.0119–0.0485 | 19–139 s |

Filling the wiki once cost $0.0235 (19 pages, 172 s).

What it shows:

- The cheap model solved the task every time, for under a cent and in under a
  minute.
- The wiki did not help on this task: on flash it added tokens, time and a little
  cost for the same result, and on pro two of the three wiki runs failed. The
  task changes one function in a package of about 8k lines, so there is little
  for a knowledge layer to save.

Limits: three runs per cell. Both failed runs named the new parameter `mode`, as
the issue text suggests, while the hidden test uses Flask's final name, `text` —
so the task partly measures recall of the real API. SWE-bench tasks may be in the
models' training data.

## 1. One feature, one model, four setups

A UI feature ("clear all" with a five-second undo) in a private production
monorepo: 23 Nx projects, about 350k lines.

- **Model:** claude-sonnet-5 in all four setups.
- **bare** is plain `claude -p`; **makedo** adds the makedo prompt and its
  wiki tools.
- **wiki** means the project wiki was filled in beforehand by makedo on
  deepseek-v4-pro, for about $0.15.
- Agents could edit files but not run commands, so tests were run by hand after
  each run.

| Setup | Run | Time, min | Cost | Diff, lines | Tests |
|---|---|---|---|---|---|
| bare, no wiki | 1 | 9.4 | $4.11 | 62 | ✓ 5/5 |
| bare, no wiki | 2 | 12.1 | $5.22 | 133 | ✓ 3/3 |
| bare + wiki | 1 | 24.0 | $1.76 | 24 | ✗ no tests written |
| bare + wiki | 2 | 7.9 | $4.32 | 190 | ✗ 1 of 4 fails |
| makedo, no wiki | 1 | 5.5 | ~$2.86 \* | 58 | ✓ 5/5 |
| makedo, no wiki | 2 | 9.1 | $4.45 | 115 | ✓ 10/10 |
| **makedo + wiki** | 1 | **5.8** | ~$3.13 \* | 42 | ✓ 4/4 |
| **makedo + wiki** | 2 | **6.0** | $3.50 | 101 | ✓ 3/3 |

\* Estimated from the transcripts at list prices.

What it shows:

- makedo + wiki was the fastest setup and the only stable one: 5.8 and 6.0
  minutes, against 8–24 minutes for the others.
- It produced the smallest diff among the setups that shipped passing tests.
- bare + wiki had the same wiki and shipped missing or failing tests both times.
  Access to the wiki is not enough; the discipline of using it is what counts.

Limits: two runs per setup. The price varies more between runs than between
setups, so this table supports no conclusion about cost.

## 2. The same feature on a cheap model

The same task and wiki, but on deepseek-v4-pro in makedo's native loop. Unlike
section 1, the agent could run commands, so it verified its own work.

| Setup | Time | Input, uncached | Input, total | Output | Cost | Diff | Tests |
|---|---|---|---|---|---|---|---|
| no wiki | 13.8 min | 125.8k | 10.2M | 37.7k | ~$0.12 \* | 236 lines | ✓ 4/4 |
| wiki | 16.3 min | 118.1k | 10.1M | 47.7k | **$0.13** | **91 lines** | ✓ 4/4, plus the module's full suite of 1,214 tests, type check and lint |

\* Not captured during the run; computed from list prices.

What it shows:

- A complete feature with tests and self-checking cost about $0.13, which is
  27–40 times less than the sonnet runs in section 1 ($3.50–5.22).
- The wiki did not reduce token use here: the budget went into running test
  suites, not into exploring the code. It did make the diff 2.6 times smaller.

Limits: one run per setup, and a different harness from section 1.

## 3. Three cheap models, nine small tasks

Nine tasks on small synthetic projects: questions answered from the wiki, bug
fixes described by symptom with hidden acceptance tests, and features with a
fixed interface. Same harness for all: makedo's native loop, a warmed-up wiki,
a fresh clone per task.

| Model | Passed | Time, whole set | Cost, whole set |
|---|---|---|---|
| deepseek-v4-flash | 9/9 | 3.5 min | $0.013 |
| deepseek-v4-pro | 9/9 | 6.9 min | $0.040 |
| qwen3.8 27b, local (Ollama) | 9/9 | 14.9 min | $0 |

What it shows: on this class of task the models differ only in speed and price.

Limits: one run per model and task. A set that every model passes does not
separate strong models from weak ones; it is a lower bar, not a ceiling.

## 4. A real backlog task, five setups

A task from a real backlog in a private production frontend: an action-logging
middleware. Acceptance has three stages: the file exists and is registered, the
type check passes, and at runtime the middleware logs what it should and nothing
else.

- makedo setups ran the native loop with a wiki warmed up by deepseek-v4-flash
  for $0.016.
- The `claude -p` setups ran without makedo and without the wiki.

| Setup | Accepted | Time | Input | Output | Cost |
|---|---|---|---|---|---|
| makedo + deepseek-v4-flash + wiki | ✓ | 3.1 min | 1.40M | 17.7k | **$0.014** |
| makedo + deepseek-v4-pro + wiki | ✓ † | 5.9 min | — | — | $0.036 |
| makedo + qwen3.8 27b, local, reasoning off + wiki | ✓ | 14.9 min | 2.04M | 25.8k | **$0** |
| `claude -p`, sonnet 5 | ✓ | 4.9 min | 4.84M | 22.5k | $2.19 |
| `claude -p`, opus 5 | ✓ | 4.7 min | 2.69M | 18.8k | $2.56 |

† The first deepseek-v4-pro run (5.0 min, 1.26M input, 13.9k output, $0.031) was
marked as failed because of a bug in the acceptance script, which did not pass
one field of the framework's API. After the script was fixed, the re-run shown
here passed.

What it shows:

- With the same acceptance, flash with a wiki solved for $0.014 what sonnet and
  opus solved for $2.19–2.56: a difference of 150–180 times.
- A local model completed a real task for $0, but only with reasoning switched
  off. With reasoning on, the first attempt ran for over 36 minutes at full load
  and did not finish.

Limits: one run per setup, except flash, which ran twice and passed both times
($0.014 and $0.016). One dependency was installed in advance. The acceptance
script calls the middleware directly and does not start the real server.
