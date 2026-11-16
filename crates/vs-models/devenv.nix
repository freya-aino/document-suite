# vs-models: a local OpenAI-compatible endpoint for development.
#
# LiteLLM is a *script*, not a `processes.` entry, deliberately: it pulls a Python stack
# and only matters when you are exercising Stage 1 or Stage 2 against real weights, so
# `devenv up` should not start it for everyone. Run `vs-litellm-up` when you want it.
#
# The proxy itself serves nothing — it routes to whatever is behind `api_base` in
# config/litellm.yaml (a local vLLM, Ollama, or a hosted API).
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
    echo "  cargo run -p vs-worker -- --role page   # with VS_MODELS or --models"
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
