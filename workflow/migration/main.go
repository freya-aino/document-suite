package migration

import (
	"context"
	"fmt"
	"os"

	"github.com/golang-migrate/migrate/v4"
	// _ "github.com/golang-migrate/migrate/v4/database/postgres"
	// _ "github.com/golang-migrate/migrate/v4/source/github"
)

func runMigration(_ context.Context, version uint) error {

	pgUseer := os.Getenv("POSTGRES_USER")
	pgPassword
	pgAddress := os.Getenv("POSTGRES_ADDRESS")
	pgDatabase := os.Getenv("POSTGRES_DATABASE")

	pgConnectionString := fmt.Sprintln("postgres://%s:%s@%s:5432/pgDatabase", pgUser, pgPassword, pgAddress, pgDatabase)

	m, err := migrate.New(
		"file:///migrations",
		pgConnectionString,
	)
	if err != nil {
		return err
	}

	m.Migrate(version)

	return nil
}
