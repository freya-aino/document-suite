package activities

import (
	"bytes"
	"context"
	"crypto/md5"
	"encoding/hex"
	"io"
	"log"
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

func S3_PUT(ctx context.Context, bucket_name string, file_id string, file []byte) error {

	client := src.S3Client()
	data := bytes.NewReader(file)

	// put object into S3
	_, err := client.PutObject(ctx, &s3.PutObjectInput{
		Bucket: aws.String(bucket_name),
		Key:    aws.String(file_id),
		Body:   data,
	})
	if err != nil {
		log.Println("upload object failed: ", err)
		return err
	}

	return nil
}

func ComputeFileHash(ctx context.Context, file []byte) (string, error) {
	sum := md5.Sum(file)
	hashSum := hex.EncodeToString(sum[:])
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
