package main

import (
	"log"
	"net/http"
	"workflow-worker/activities"
	"workflow-worker/src"
	"workflow-worker/workflows"

	"github.com/gin-gonic/gin"
	"go.temporal.io/sdk/client"
	"go.temporal.io/sdk/worker"
)

func startWorker(task_queue_name string) {
	client, err := client.Dial(src.LoadTemporalConfigs("test"))
	if err != nil {
		log.Fatalln("Unable to create client", err)
	}
	defer client.Close()

	worker_ := worker.New(client, task_queue_name, worker.Options{})
	defer worker_.Stop()

	worker_.RegisterWorkflow(workflows.HealthCheckWorkflow)
	worker_.RegisterWorkflow(workflows.IngresFileWorkflow)

	worker_.RegisterActivity(activities.S3_GET)
	worker_.RegisterActivity(activities.S3_PUT)
	worker_.RegisterActivity(activities.ComputeFileHash)
	worker_.RegisterActivity(activities.CheckHealth)

	log.Println("Registered workflows and activities")

	err = worker_.Run(worker.InterruptCh())
	if err != nil {
		log.Fatalln("Unable to start worker", err)
	}
}

func main() {

	// start temporal worker
	taskQueueName := "main-task-queue"
	go startWorker(taskQueueName)
	log.Println("Worker goroutine started")

	// create http router
	router := gin.Default()
	// router.GET("/list_bucket", func(c *gin.Context) {
	// 	tmpID := fmt.Sprintf("list_bucket-%d", time.Now().UnixNano())
	// 	go src.StartWorkflow(
	// 		client.StartWorkflowOptions{
	// 			ID:        tmpID,
	// 			TaskQueue: TASK_QUEUE_NAME,
	// 		},
	// 		src.OCRBucket,
	// 		"documents", // bucket name
	// 	)

	// 	c.JSON(http.StatusOK, gin.H{"content": "workflow started"})
	// })
	// router.GET("/vectorize", func(c *gin.Context) {

	// 	name := c.Query("name")

	// 	log.Println("Tryping to vectorize ", name)

	// 	go src.StartWorkflow(
	// 		client.StartWorkflowOptions{
	// 			ID:        tmpID,
	// 			TaskQueue: TASK_QUEUE_NAME,
	// 		},
	// 		src.VectorizeObjectFromS3,
	// 		"documents", // bucket name
	// 		name,
	// 	)

	// 	c.JSON(http.StatusOK, gin.H{"content": "workflow started"})
	// })

	router.GET("/health", func(c *gin.Context) {

		_, err := src.StartWorkflow(taskQueueName, workflows.HealthCheckWorkflow)
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"status": "unhealthy"})
			return
		}

		c.JSON(http.StatusOK, gin.H{"status": "healthy"})
	})

	router.POST("/upload", func(c *gin.Context) {

		file, err := c.FormFile("file")
		if err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": "no file"})
			return
		}

		// // check extension
		// // TODO filter by file type
		// extension := filepath.Ext(f.Filename)
		// if extension != ".png" && extension != ".jpg" { // && extension != ".pdf" && extension != ".docx" {
		// 	c.JSON(http.StatusBadRequest, gin.H{"error": fmt.Sprintf("unsupported type: %s", extension)})
		// 	return
		// }

		// extract byte data
		fileData, err := src.GetFileData(file)
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": err})
		}

		// write to tmp
		tmpPath, err := src.WriteAsTemp(&fileData)
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": err})
		}

		// run workflow
		_, err = src.StartWorkflow(
			taskQueueName,
			workflows.IngresFileWorkflow,
			"documents", // bucket_name
			tmpPath,
		)
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": "workflow error"})
			return
		}

		c.JSON(http.StatusOK, gin.H{})
	})

	router.Run("0.0.0.0:8080")
}
