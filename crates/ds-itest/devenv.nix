# ds-itest: the integration suite's entry points.
#
# Scripts rather than a Makefile, because this repository uses devenv and not make. They
# are thin on purpose — each is the command the crate's README documents, so that running
# the suite and reading about running it cannot diverge.
{ config, ... }:

{
  scripts = {
    # Bring the stack up and print the variables the harness reads. Sourcing the output is
    # the second half: `eval "$(ds-itest-env)"`.
    ds-itest-up.exec = ''
      "${config.devenv.root}/deploy/compose/stack.sh" up "$@"
    '';
    ds-itest-down.exec = ''
      "${config.devenv.root}/deploy/compose/stack.sh" down "$@"
    '';
    ds-itest-env.exec = ''
      "${config.devenv.root}/deploy/compose/stack.sh" env
    '';

    # The functional suite. `--ignored` because every test here needs the stack, which is
    # the house convention, and `stack_or_skip` means this is still safe without Docker.
    ds-itest.exec = ''
      set -euo pipefail
      eval "$("${config.devenv.root}/deploy/compose/stack.sh" env)"
      cargo test -p ds-itest --test smoke --test fidelity -- --ignored "$@"
    '';

    # Latency runs alone: the service takes one document at a time, so another test's
    # upload would queue inside the measurement.
    ds-latency.exec = ''
      set -euo pipefail
      eval "$("${config.devenv.root}/deploy/compose/stack.sh" env)"
      cargo test -p ds-itest --test latency -- --ignored --test-threads 1 "$@"
    '';

    # Writes this host's latency baseline. A missing baseline makes the latency test
    # report and skip rather than fail, so this is what turns it on for a new machine —
    # and it must be run on an idle host, because everything after it is a ratio to it.
    ds-baseline-write.exec = ''
      set -euo pipefail
      eval "$("${config.devenv.root}/deploy/compose/stack.sh" env)"
      DS_ITEST_WRITE_BASELINE=1 cargo test -p ds-itest --test latency -- --ignored --test-threads 1 --nocapture
    '';

    # The whole chain offline: corpus, candidates, runs, scores, a list and a report.
    # `--release` is not optional — `page.denoise`'s median filter takes 2.1 s released
    # and 36.4 s in debug, past its own 60 s timeout once decode and re-encode are added,
    # so a debug chain scores every image conditioner as a failure and biases the search
    # against exactly the templates it exists to compare.
    ds-fidelity.exec = ''
      set -euo pipefail
      root="${config.devenv.root}"
      out="''${1:-$root/target/fidelity}"
      models="''${DS_ADAPT_MODELS:-$root/config/models.example.toml}"
      mkdir -p "$out"
      cargo build --release -p ds-adapt -p ds-ingest --bin ds-adapt --bin ds-render-child
      bin="$root/target/release"
      "$bin/ds-adapt" dataset --out "$out/corpus" --docs-per-shape 8 --rotate 0,90 \
        --scan-dpi 200 --render-child "$bin/ds-render-child"
      "$bin/ds-adapt" generate --search-space "$root/config/search-space.itest.json" \
        --models "$models" --out "$out/candidates.json"
      "$bin/ds-adapt" run --manifest "$out/corpus/manifest.jsonl" --candidates "$out/candidates.json" \
        --models "$models" --out "$out/runs" --render-child "$bin/ds-render-child" --parallel 1
      "$bin/ds-adapt" score --runs "$out/runs" --out "$out/evals.jsonl" --models "$models"
      "$bin/ds-adapt" select --evals "$out/evals.jsonl" --candidates "$out/candidates.json" \
        --version "fidelity-$(date -u +%Y%m%dT%H%M%SZ)" --out "$out/dag-list.json" \
        --current "$root/config/dag-list.example.json" --report "$out/report-train.md" --split train
      # The confirmation pass: the same list, reported over the pages it was *not* chosen
      # on. Only this one is quotable — `report-train.md` is a screen of C candidates, and
      # its own header prints how much ranking C of them on n pages inflates the winner.
      "$bin/ds-adapt" report --list "$out/dag-list.json" --evals "$out/evals.jsonl" \
        --current "$root/config/dag-list.example.json" --out "$out/report-holdout.md" --split holdout
      echo "screen:       $out/report-train.md"
      echo "confirmation: $out/report-holdout.md"
    '';
  };
}
