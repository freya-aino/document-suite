package activities

import (
	"context"
	"errors"
	"net/http"
	"os"
	"time"
	"workflow/core"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/s3"

	awshttp "github.com/aws/aws-sdk-go-v2/aws/transport/http"
)

func GetAllS3ObjectIDsInBucket(ctx context.Context, bucketName string) ([]string, error) {

	client, err := core.S3Client()
	if err != nil {
		return []string{}, err
	}

	resp, err := client.ListObjectsV2(ctx, &s3.ListObjectsV2Input{
		Bucket: aws.String(bucketName),
	})
	if err != nil {
		return []string{}, err
	}

	var out []string
	for _, c := range resp.Contents {
		out = append(out, string(*c.Key))
	}

	return out, nil
}

func S3GetPresignedDocumentURL(ctx context.Context, bucketName string, documentId string, expirationSeconds int) (string, error) {
	client, err := core.S3Client()
	if err != nil {
		return "", err
	}

	expires := time.Duration(expirationSeconds) * time.Second
	presignClient := s3.NewPresignClient(client)
	presigned, err := presignClient.PresignGetObject(
		ctx,
		&s3.GetObjectInput{
			Bucket: aws.String(bucketName),
			Key:    aws.String(documentId),
		},
		s3.WithPresignExpires(expires),
	)
	if err != nil {
		return "", err
	}

	location := presigned.URL

	return location, nil
}

func S3PutDocument(ctx context.Context, bucketName string, documentId string, tmpPath string) error {

	// create client
	client, err := core.S3Client()
	if err != nil {
		return err
	}

	// open document buffer
	document, err := os.Open(tmpPath)
	if err != nil {
		return err
	}

	// put object into S3
	_, err = client.PutObject(ctx, &s3.PutObjectInput{
		Bucket: aws.String(bucketName),
		Key:    aws.String(documentId),
		Body:   document,
	})
	if err != nil {
		return err
	}

	return nil
}

func S3DocumentExists(ctx context.Context, bucketName string, documentId string) (bool, error) {

	// create client
	client, err := core.S3Client()
	if err != nil {
		return true, err
	}

	// get object from S3 if available
	_, err = client.HeadObject(ctx, &s3.HeadObjectInput{
		Bucket: aws.String(bucketName),
		Key:    aws.String(documentId),
	})
	if err != nil {

		var responseError *awshttp.ResponseError
		if errors.As(err, &responseError) && responseError.ResponseError.HTTPStatusCode() == http.StatusNotFound {
			return false, nil
		}
		return true, nil
	}

	return true, nil
}
