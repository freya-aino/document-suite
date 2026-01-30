package activities

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"io"
	"log"
	"os"
	"workflow-worker/src"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/s3"
)

func CheckHealth(ctx context.Context) error {
	log.Println("helath check")
	return nil
}

func GetAllS3ObjectIDsInBucket(ctx context.Context, bucket_name string) ([]string, error) {

	client := src.S3Client()

	resp, err := client.ListObjectsV2(ctx, &s3.ListObjectsV2Input{
		Bucket: aws.String(bucket_name),
	})
	if err != nil {
		log.Println("Listing Objects from bucket failed: ", err)
		return []string{}, err
	}

	var out []string
	for _, c := range resp.Contents {
		out = append(out, string(*c.Key))
	}

	return out, nil
}

func S3_GET(ctx context.Context, bucket_name string, obj_name string) ([]byte, error) {
	client := src.S3Client()

	resp, err := client.GetObject(ctx, &s3.GetObjectInput{
		Bucket: aws.String(bucket_name),
		Key:    aws.String(obj_name),
	})
	if err != nil {
		log.Println("Failed to get object from bucket: ", err)
		return []byte{}, err
	}
	defer resp.Body.Close()

	data, err := io.ReadAll(resp.Body)
	if err != nil {
		log.Println("Failed to read response body from S3 Object")
		return []byte{}, err
	}

	return data, nil
}

func S3_PUT(ctx context.Context, bucket_name string, file_id string, filePath string) error {

	// create client
	client := src.S3Client()

	// open file buffer
	file, err := os.Open(filePath)
	if err != nil {
		return err
	}

	// put object into S3
	_, err = client.PutObject(ctx, &s3.PutObjectInput{
		Bucket: aws.String(bucket_name),
		Key:    aws.String(file_id),
		Body:   file,
	})
	if err != nil {
		return err
	}

	return nil
}

func S3_CHECK_DUPLICATE(ctx context.Context, bucket_name string, file_id string) (bool, error) {

	// create client
	client := src.S3Client()

	// get object from S3
	_, err := client.GetObject(ctx, &s3.GetObjectInput{
		Bucket: aws.String(bucket_name),
		Key:    aws.String(file_id),
	})
	if err != nil {
		return false, nil
	}

	return true, nil
}

func ComputeFileHash(ctx context.Context, filePath string) (string, error) {

	// open file buffer
	file, err := os.Open(filePath)
	if err != nil {
		return "", err
	}
	defer file.Close()

	hasher := sha256.New()
	_, err = io.Copy(hasher, file)
	if err != nil {
		return "", err
	}
	hashSum := hex.EncodeToString(hasher.Sum(nil))
	return hashSum, nil
}

// func RabbitMQ()
// conn, err := amqp.Dial("amqp://guest:guest@localhost:5672/")
// if err != nil {
// 	log.Fatalln("Unable to dial rabbitmq server")
// }
// defer conn.Close()

// ch, err := conn.Channel()
// if err != nil {
// 	log.Fatalln("Unable to open channel")
// }
// defer ch.Close()

// q, err := ch.QueueDeclare(
// 	"hello", // name
// 	false,   // durable
// 	false,   // delete when unused
// 	false,   // exclusive
// 	false,   // no-wait
// 	nil,     // arguments
// )
// if err != nil {
// 	log.Fatalln("Failed to declare a queue")
// }

// ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
// defer cancel()

// err = ch.PublishWithContext(
// 	ctx,
// 	"",     // exchange
// 	q.Name, // routing key
// 	false,  // mandatory
// 	false,  // immediate
// 	amqp.Publishing{
// 		ContentType: "text/plain",
// 		Body:        []byte("Hello World"), // TODO
// 	},
// )
// if err != nil {
// 	log.Fatalln("Failed to publish message to queue")
// }

// 	return "Hello world", nil
// }
