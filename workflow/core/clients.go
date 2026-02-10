package core

import (
	"database/sql"
	"errors"
	"os"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/credentials"
	"github.com/aws/aws-sdk-go-v2/service/s3"

	"github.com/qdrant/go-client/qdrant"
)

func PostgresClient() (*sql.DB, error) {
	// TODO - ?sslmode=enable
	connStr, err := PGConnectionString()
	if err != nil {
		return nil, err
	}
	db, err := sql.Open("postgres", connStr)
	if err != nil {
		return nil, err
	}
	return db, nil

}

func S3Client() (*s3.Client, error) {

	region := os.Getenv("AWS_REGION")
	endpoint := os.Getenv("S3_ENDPOINT_URL")
	accessKeyID := os.Getenv("RUSTFS_ACCESS_KEY")
	secretAccessKey := os.Getenv("RUSTFS_SECRET_KEY")

	if accessKeyID == "" {
		return nil, errors.New("S3 accessKeyID env variable not set")
	}
	if secretAccessKey == "" {
		return nil, errors.New("S3 secretAccessKey env variable not set")
	}
	if region == "" {
		return nil, errors.New("S3 region env variable not set")
	}
	if endpoint == "" {
		return nil, errors.New("S3 endpoint env variable not set")
	}

	// build aws.Config
	cfg := aws.Config{
		Region:      region,
		Credentials: aws.NewCredentialsCache(credentials.NewStaticCredentialsProvider(accessKeyID, secretAccessKey, "")),
	}

	client := s3.NewFromConfig(cfg, func(o *s3.Options) {
		o.UsePathStyle = true
		o.BaseEndpoint = aws.String(endpoint)
	})

	return client, nil
}

func QdrantClient() (*qdrant.Client, error) {

	client, err := qdrant.NewClient(&qdrant.Config{
		Host: "qdrant",
		Port: 6334,
		// UseTLS: , # TODO
		// APIKey: , # TODO
	})
	if err != nil {
		return nil, err
	}
	return client, err
}
