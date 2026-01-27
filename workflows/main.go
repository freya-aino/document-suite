package main

import (
	"fmt"
	"log"
	"net/http"
	"time"

	"go.temporal.io/sdk/client"
	"go.temporal.io/sdk/worker"

	"github.com/gin-gonic/gin"

	src "workflows/src"
)

var TASK_QUEUE_NAME = "main-task-queue"

func startWorker() {
	client, err := client.Dial(src.LoadTemporalConfigs("test"))
	if err != nil {
		log.Fatalln("Unable to create client", err)
	}
	defer client.Close()

	w := worker.New(client, TASK_QUEUE_NAME, worker.Options{})
	defer w.Stop()

	w.RegisterWorkflow(src.OCRBucket)
	w.RegisterWorkflow(src.VectorizeObjectFromS3)

	w.RegisterActivity(src.GetAllS3ObjectIDsInBucket)
	w.RegisterActivity(src.LoadObjectFromS3)

	log.Println("Registered workflows and activities")

	err = w.Run(worker.InterruptCh())
	if err != nil {
		log.Fatalln("Unable to start worker", err)
	}
}

func main() {

	go startWorker()
	log.Println("Worker goroutine started")

	router := gin.Default()
	router.GET("/list_bucket", func(c *gin.Context) {
		tmpID := fmt.Sprintf("list_bucket-%d", time.Now().UnixNano())
		go src.StartWorkflow(
			client.StartWorkflowOptions{
				ID:        tmpID,
				TaskQueue: TASK_QUEUE_NAME,
			},
			src.OCRBucket,
			"documents", // bucket name
		)

		c.JSON(http.StatusOK, gin.H{"content": "workflow started"})
	})
	router.GET("/vectorize", func(c *gin.Context) {

		name := c.Query("name")

		log.Println("Tryping to vectorize ", name)

		tmpID := fmt.Sprintf("vectorize_obj-%d", time.Now().UnixNano())
		go src.StartWorkflow(
			client.StartWorkflowOptions{
				ID:        tmpID,
				TaskQueue: TASK_QUEUE_NAME,
			},
			src.VectorizeObjectFromS3,
			"documents", // bucket name
			name,
		)

		c.JSON(http.StatusOK, gin.H{"content": "workflow started"})
	})
	router.Run("0.0.0.0:10000")
}
