package activities

import (
	"context"
	"errors"
	"fmt"
	"os"
	"workflow/core"

	"github.com/samborkent/uuidv7"
)

func PGCreateDocument(ctx context.Context, tableName string, s3BucketName string, documentUUID uuidv7.UUID) error {

	pgdb := os.Getenv("POSTGRES_DB")
	if pgdb == "" {
		return errors.New("'POSGRES_DB' environment variable not found")
	}

	client, err := core.PostgresClient()
	if err != nil {
		return err
	}
	defer client.Close()

	_, err = client.Query(fmt.Sprintf(
		"INSERT INTO %s", tableName,
	))
	if err != nil {
		return err
	}

	return nil
}

// func PGDocumentExists(ctx context.Context, )
