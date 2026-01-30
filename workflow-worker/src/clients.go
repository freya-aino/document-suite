package src

import (
	"fmt"
	"log"
	"os"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/credentials"
	"github.com/aws/aws-sdk-go-v2/service/s3"

	"github.com/qdrant/go-client/qdrant"
)

func S3Client() *s3.Client {

	region := os.Getenv("AWS_REGION")
	endpoint := fmt.Sprintf("http://%s:%s", os.Getenv("RUSTFS_ADDRESS"), os.Getenv("RUSTFS_PORT"))
	accessKeyID := os.Getenv("RUSTFS_ACCESS_KEY")
	secretAccessKey := os.Getenv("RUSTFS_SECRET_KEY")

	if accessKeyID == "" {
		log.Fatalln("S3 accessKeyID env variable not set")
	}
	if secretAccessKey == "" {
		log.Fatalln("S3 secretAccessKey env variable not set")
	}
	if region == "" {
		log.Fatalln("S3 region env variable not set")
	}
	if endpoint == "" {
		log.Fatalln("S3 endpoint env variable not set")
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

	return client
}

func QdrantClient() *qdrant.Client {

	client, err := qdrant.NewClient(&qdrant.Config{
		Host: "qdrant",
		Port: 6334,
	})
	if err != nil {
		log.Fatalln("Unable to open Qdrant Client: ", err)
	}
	return client
}
