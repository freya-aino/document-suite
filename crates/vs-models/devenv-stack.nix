# vs-models' model tier: a local OpenAI-compatible endpoint. Part of the `stack` profile
# only — LiteLLM pulls a whole Python stack, which has no business in a shell whose job is
# to build the suite.
#
# Still a *script* rather than a `processes.` entry, even inside the profile: it only
# matters when you are exercising Stage 1 or Stage 2 against real weights, and it serves
# nothing itself — it routes to whatever is behind `api_base` in config/litellm.yaml (a
# local vLLM, Ollama, or a hosted API).
{ pkgs, ... }:

{
  packages = [ pkgs.litellm ];

  scripts.vs-litellm-up.exec = ''
    config="$DEVENV_ROOT/config/litellm.yaml"
    if [ ! -f "$config" ]; then
      echo "missing $config" >&2
      exit 1
    fi
    echo "litellm proxy on :4000 — point the suite at config/models.litellm.toml"
    echo "  cargo run -p vs-suite          # point [models] registry at config/models.litellm.toml"
    exec ${pkgs.litellm}/bin/litellm --config "$config" --port 4000 "$@"
  '';

  scripts.vs-litellm-check.exec = ''
    # Which aliases the proxy is actually serving. The suite's registry asks for these
    # names, so a mismatch here is the first thing to check when a step reports an
    # unknown model.
    ${pkgs.curl}/bin/curl -sS http://127.0.0.1:4000/v1/models || {
      echo "proxy not reachable on :4000 — start it with vs-litellm-up" >&2
      exit 1
    }
  '';
}
