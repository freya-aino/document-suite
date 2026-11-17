# Keycloak for development. Part of the `stack` profile only.
#
# `vs-api` refuses to start without a reachable realm (`[auth]` in its config), so this is
# not an optional extra of the stack — it is the difference between the API starting and
# not. The realm in `realm-dev.json` is imported on every start: a client with an audience
# mapper, the organization claim, the `vs-admin` role, and a second client in another
# organization for checking isolation. See `README.md` next to this file.
#
# The official image rather than `pkgs.keycloak`, for the same reason as Qdrant: nixpkgs'
# build drags a JVM and its own service wiring into the shell, and realm import is a
# documented flag on the image. `docker` comes from the host; if it is missing this
# process says so rather than failing evaluation.
{ pkgs, config, ... }:

let
  port = 8081; # 8080 is vs-api's own default
  image = "quay.io/keycloak/keycloak:26.4";
  name = "vs-dev-keycloak";
  realm = "${config.devenv.root}/deploy/keycloak/realm-dev.json";
  issuer = "http://127.0.0.1:${toString port}/realms/koki";
in
{
  packages = [ pkgs.jq ];

  processes.keycloak.exec = ''
    if ! command -v docker >/dev/null; then
      echo "keycloak needs docker on PATH (see deploy/keycloak/devenv.nix)" >&2
      exit 1
    fi
    docker rm -f ${name} >/dev/null 2>&1 || true
    exec docker run --rm --name ${name} \
      -p 127.0.0.1:${toString port}:8080 \
      -e KC_BOOTSTRAP_ADMIN_USERNAME=admin \
      -e KC_BOOTSTRAP_ADMIN_PASSWORD=admin \
      -v ${realm}:/opt/keycloak/data/import/realm-dev.json:ro \
      ${image} start-dev --import-realm
  '';

  # `docker run --rm` cleans up on SIGTERM, but a hard kill of devenv does not reach it.
  processes.keycloak.process-compose.shutdown.command = "docker stop ${name}";

  # Ready means *the realm answers*, not that the port is open: the import runs after the
  # server starts, and a token request in between fails with a 404 that looks like a
  # misconfiguration. Keycloak takes ~20 s from cold.
  processes.keycloak.process-compose.readiness_probe = {
    http_get = {
      host = "127.0.0.1";
      port = port;
      path = "/realms/koki/.well-known/openid-configuration";
    };
    initial_delay_seconds = 5;
    period_seconds = 2;
    failure_threshold = 45;
  };

  env.VS_KEYCLOAK_ISSUER = issuer;

  scripts.vs-token = {
    description = "Print a dev-realm access token: vs-token [t1|t2]";
    exec = ''
      case "''${1:-t1}" in
        t1) client=vs-api;    secret=vs-api-dev-secret ;;
        t2) client=vs-api-t2; secret=vs-api-t2-dev-secret ;;
        *)  echo "usage: vs-token [t1|t2]   (t1 holds vs-admin, t2 holds no role)" >&2; exit 1 ;;
      esac
      ${pkgs.curl}/bin/curl -sS -X POST "${issuer}/protocol/openid-connect/token" \
        -d grant_type=client_credentials -d "client_id=$client" -d "client_secret=$secret" \
        | ${pkgs.jq}/bin/jq -er '.access_token' \
        || { echo "no token from ${issuer} — is the stack profile up?" >&2; exit 1; }
    '';
  };
}
