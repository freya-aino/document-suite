# ds-vault: the client side of Postgres — migrations and the connection the tests use.
# The Postgres *service* is in `devenv-stack.nix` (the `stack` profile), so this shell
# can be pointed at any database: override `DATABASE_URL` and
# `DS__POSTGRES__{URL,PASSWORD}`.
{ pkgs, config, ... }:

{
  packages = [ pkgs.sqlx-cli ];

  env = {
    # Schema owner: `ds-migrate`, `ds-suite migrate`, tests. Defaults to the `stack`
    # profile's Postgres.
    DATABASE_URL = "postgres://ds:ds@127.0.0.1:5432/document_suite";
    # Runtime role password for the service (config/worker.dev.toml connects as ds_app;
    # DS__SECTION__KEY keeps it out of the TOML).
    DS__POSTGRES__PASSWORD = "ds_app";
  };

  scripts.ds-migrate = {
    description = "Apply ds-vault migrations to the database in DATABASE_URL";
    exec = ''sqlx migrate run --source "${config.devenv.root}/crates/ds-vault/migrations" "$@"'';
  };
}
