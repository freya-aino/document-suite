# ds-vault's service: Postgres. Part of the `stack` profile only
# (`devenv --profile stack up`), so the default shell builds the suite without
# provisioning a database. The client side of this — `sqlx-cli`, `DATABASE_URL` and
# `ds-migrate` — stays in `devenv.nix`, because pointing those at a database somewhere
# else is exactly what an external deployment does.
{ pkgs, ... }:

{
  services.postgres = {
    enable = true;
    package = pkgs.postgresql_17;
    listen_addresses = "127.0.0.1";
    port = 5432;
    initialDatabases = [
      { name = "document_suite"; user = "ds"; pass = "ds"; }
    ];
  };

  # Roles: `ds` owns the schemas and runs migrations (and `#[sqlx::test]`, which
  # creates a throwaway database per test, hence CREATEDB); `ds_app` is the runtime role
  # of the API and workers (insert-only on the vault, granted by migration 0003).
  # Runs as the local superuser (the OS user) once Postgres is up.
  tasks."ds-vault:roles" = {
    exec = ''
      for _ in $(seq 1 30); do
        psql -h 127.0.0.1 -d postgres -tAc "SELECT 1 FROM pg_roles WHERE rolname = 'ds'" | grep -q 1 && break
        sleep 1
      done
      psql -h 127.0.0.1 -d postgres -c "ALTER ROLE ds CREATEDB"
      psql -h 127.0.0.1 -d postgres -tAc "SELECT 1 FROM pg_roles WHERE rolname = 'ds_app'" | grep -q 1 \
        || psql -h 127.0.0.1 -d postgres -c "CREATE ROLE ds_app LOGIN PASSWORD 'ds_app'"
    '';
    after = [ "devenv:processes:postgres@ready" ];
    wantedBy = [ "devenv:processes:postgres" ];
  };
}
