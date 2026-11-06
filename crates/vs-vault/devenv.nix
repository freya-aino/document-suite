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

  # `#[sqlx::test]` creates a throwaway database per test; the app role needs CREATEDB.
  # Runs as the local superuser (the OS user) once Postgres is up.
  tasks."vs-vault:grant-createdb" = {
    exec = ''
      for _ in $(seq 1 30); do
        psql -h 127.0.0.1 -d postgres -tAc "SELECT 1 FROM pg_roles WHERE rolname = 'vs'" | grep -q 1 && break
        sleep 1
      done
      psql -h 127.0.0.1 -d postgres -c "ALTER ROLE vs CREATEDB"
    '';
    after = [ "devenv:processes:postgres@ready" ];
    wantedBy = [ "devenv:processes:postgres" ];
  };

  scripts.vs-migrate = {
    description = "Apply vs-vault migrations to the local database";
    exec = ''sqlx migrate run --source "${config.devenv.root}/crates/vs-vault/migrations" "$@"'';
  };
}
