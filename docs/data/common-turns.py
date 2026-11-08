"""Run with the minmax-bench environment from the root of its checkout."""

from minmax_bench.report.buckets import summarize
from minmax_bench.runstore import RunStore

run = RunStore.open("runs", "202f98bd-a2f1-4390-8307-658b451b7727")
model = run.manifest.models[0]
arms = {
    strategy: {
        (row.session_id, row.index): row
        for row in run.paired_rows(model, strategy)
        if row.ok
    }
    for strategy in run.manifest.strategies
}
common = sorted(set.intersection(*(set(rows) for rows in arms.values())))
for strategy, rows in arms.items():
    print(strategy, summarize([rows[key] for key in common])[-1])
