# Ferrix scratch conventions

Generated data is disposable and must not be left on disk.

## Where generated files go

Everything a benchmark or test produces goes in `benchdata/`, which is
gitignored. Nothing else in the repo should ever be written to at runtime.

## Cleaning up

```bash
just clean-data      # remove benchdata/ (generated CSVs, .ferrix caches)
just clean-all       # the above plus target/
```

Or directly:

```bash
rm -rf benchdata
```

## Rules for contributors (and agents)

1. Benchmark data is regenerable in seconds (`gen-data`), so never keep it
   around "in case". Delete it when the measurement is done.
2. Unit tests that write files must use `std::env::temp_dir()` and remove what
   they create, even on failure.
3. Never write scratch files into the repo root or a source directory.
4. A 10GB benchmark file is not something to leave on a user's disk. Generate,
   measure, delete — in that order, in the same session.
