package activities

import (
	"context"
	"log"
	"workflow/core"
)

func PGCreateDocument(ctx context.Context, tableName string, s3BucketName string, documentUUID string) error {

	conn, err := core.PostgresClient(ctx)
	if err != nil {
		return err
	}
	defer conn.Close(ctx)

	// _, err = client.Query(fmt.Sprintf(
	// 	"INSERT INTO %s", tableName,
	// ))
	// if err != nil {
	// 	return err
	// }
	log.Println("PG Entry created (TODO)")

	return nil
}

// func PGDocumentExists(ctx context.Context, )
