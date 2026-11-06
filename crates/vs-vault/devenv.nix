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
    # Schema owner: `vs-migrate`, `vs-api migrate`, tests.
    DATABASE_URL = "postgres://vs:vs@127.0.0.1:5432/visual_suite";
    # Runtime role password for API and workers (config/worker.dev.toml connects as
    # vs_app; VS__SECTION__KEY keeps it out of the TOML).
    VS__POSTGRES__PASSWORD = "vs_app";
  };

  # Roles: `vs` owns the schemas and runs migrations (and `#[sqlx::test]`, which
  # creates a throwaway database per test, hence CREATEDB); `vs_app` is the runtime role
  # of the API and workers (insert-only on the vault, granted by migration 0003).
  # Runs as the local superuser (the OS user) once Postgres is up.
  tasks."vs-vault:roles" = {
    exec = ''
      for _ in $(seq 1 30); do
        psql -h 127.0.0.1 -d postgres -tAc "SELECT 1 FROM pg_roles WHERE rolname = 'vs'" | grep -q 1 && break
        sleep 1
      done
      psql -h 127.0.0.1 -d postgres -c "ALTER ROLE vs CREATEDB"
      psql -h 127.0.0.1 -d postgres -tAc "SELECT 1 FROM pg_roles WHERE rolname = 'vs_app'" | grep -q 1 \
        || psql -h 127.0.0.1 -d postgres -c "CREATE ROLE vs_app LOGIN PASSWORD 'vs_app'"
    '';
    after = [ "devenv:processes:postgres@ready" ];
    wantedBy = [ "devenv:processes:postgres" ];
  };

  scripts.vs-migrate = {
    description = "Apply vs-vault migrations to the local database";
    exec = ''sqlx migrate run --source "${config.devenv.root}/crates/vs-vault/migrations" "$@"'';
  };
}
