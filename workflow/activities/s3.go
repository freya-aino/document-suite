package activities

import (
	"context"
	"errors"
	"io"
	"net/http"
	"os"
	"workflow/src"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/s3"

	awshttp "github.com/aws/aws-sdk-go-v2/aws/transport/http"
)

func GetAllS3ObjectIDsInBucket(ctx context.Context, bucket_name string) ([]string, error) {

	client, err := src.S3Client()
	if err != nil {
		return []string{}, err
	}

	resp, err := client.ListObjectsV2(ctx, &s3.ListObjectsV2Input{
		Bucket: aws.String(bucket_name),
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

func S3Get(ctx context.Context, bucket_name string, obj_name string) ([]byte, error) {
	client, err := src.S3Client()
	if err != nil {
		return []byte{}, err
	}

	resp, err := client.GetObject(ctx, &s3.GetObjectInput{
		Bucket: aws.String(bucket_name),
		Key:    aws.String(obj_name),
	})
	if err != nil {
		return []byte{}, err
	}
	defer resp.Body.Close()

	data, err := io.ReadAll(resp.Body)
	if err != nil {
		return []byte{}, err
	}

	return data, nil
}

func S3Put(ctx context.Context, bucketName string, fileUUID string, filePath string) error {

	// create client
	client, err := src.S3Client()
	if err != nil {
		return err
	}

	// open file buffer
	file, err := os.Open(filePath)
	if err != nil {
		return err
	}

	// put object into S3
	_, err = client.PutObject(ctx, &s3.PutObjectInput{
		Bucket: aws.String(bucketName),
		Key:    aws.String(fileUUID),
		Body:   file,
	})
	if err != nil {
		return err
	}

	return nil
}

func S3FileExists(ctx context.Context, bucket_name string, file_id string) (bool, error) {

	// create client
	client, err := src.S3Client()
	if err != nil {
		return true, err
	}

	// get object from S3 if available
	_, err = client.HeadObject(ctx, &s3.HeadObjectInput{
		Bucket: aws.String(bucket_name),
		Key:    aws.String(file_id),
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
