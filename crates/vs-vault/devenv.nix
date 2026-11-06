# vs-vault: Postgres (ops, registry and vault schemas) + sqlx migrations.
{ pkgs, config, ... }:

{
  services.postgres = {
    enable = true;
    package = pkgs.postgresql_17;
    listen_addresses = "127.0.0.1";
    port = 5432;
    initialDatabases = [
      { name = "visual_suite"; user = "vs"; pass = "vs"; }
    ];
  };

  packages = [ pkgs.sqlx-cli ];

  env = {
    # sqlx compile-time query checks and `sqlx migrate`.
    DATABASE_URL = "postgres://vs:vs@127.0.0.1:5432/visual_suite";
    # vs-infra config keeps the password out of the TOML (VS__SECTION__KEY override).
    VS__POSTGRES__PASSWORD = "vs";
  };

  scripts.vs-migrate = {
    description = "Apply vs-vault migrations to the local database";
    exec = ''sqlx migrate run --source "${config.devenv.root}/crates/vs-vault/migrations" "$@"'';
  };
}
