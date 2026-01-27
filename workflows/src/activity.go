package src

import (
	"context"
	"io"
	"log"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/s3"
)

func GetAllS3ObjectIDsInBucket(ctx context.Context, bucket_name string) ([]string, error) {

	client := S3Client()

	resp, err := client.ListObjectsV2(ctx, &s3.ListObjectsV2Input{
		Bucket: aws.String(bucket_name),
	})
	if err != nil {
		log.Fatalln("Listing Objects from bucket failed: ", err)
		// return []string{}, nil
	}

	var out []string
	for _, c := range resp.Contents {
		out = append(out, string(*c.Key))
	}

	return out, nil
}

func LoadObjectFromS3(ctx context.Context, bucket_name string, obj_name string) ([]byte, error) {
	client := S3Client()

	resp, err := client.GetObject(ctx, &s3.GetObjectInput{
		Bucket: aws.String(bucket_name),
		Key:    aws.String(obj_name),
	})
	if err != nil {
		log.Fatalln("Failed to get object from bucket: ", err)
	}
	defer resp.Body.Close()

	data, err := io.ReadAll(resp.Body)
	if err != nil {
		log.Fatalln("Failed to read response body from S3 Object")
	}
	return data, nil
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
