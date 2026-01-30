package activities

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"io"
	"log"
	"os"
)

func CheckHealth(ctx context.Context) error {
	log.Println("helath check")
	return nil
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
