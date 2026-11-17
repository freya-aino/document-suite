# vs-vault: the client side of Postgres — migrations and the connection the tests use.
# The Postgres *service* is in `devenv-stack.nix` (the `stack` profile), so this shell
# can be pointed at any database: override `DATABASE_URL` and
# `VS__POSTGRES__{URL,PASSWORD}`.
{ pkgs, config, ... }:

{
  packages = [ pkgs.sqlx-cli ];

  env = {
    # Schema owner: `vs-migrate`, `vs-api migrate`, tests. Defaults to the `stack`
    # profile's Postgres.
    DATABASE_URL = "postgres://vs:vs@127.0.0.1:5432/visual_suite";
    # Runtime role password for API and workers (config/worker.dev.toml connects as
    # vs_app; VS__SECTION__KEY keeps it out of the TOML).
    VS__POSTGRES__PASSWORD = "vs_app";
  };

  scripts.vs-migrate = {
    description = "Apply vs-vault migrations to the database in DATABASE_URL";
    exec = ''sqlx migrate run --source "${config.devenv.root}/crates/vs-vault/migrations" "$@"'';
  };
}
