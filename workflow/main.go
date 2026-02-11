package main

import (
	"log"
	"net/http"
	"os"
	"workflow/activities"
	"workflow/core"
	"workflow/workflows"

	"github.com/gin-gonic/gin"
	"go.temporal.io/sdk/client"
	"go.temporal.io/sdk/worker"
)

func startWorker(task_queue_name string) {
	client, err := client.Dial(core.LoadTemporalConfigs())
	if err != nil {
		log.Fatalln("Unable to create client", err)
	}
	defer client.Close()

	worker_ := worker.New(client, task_queue_name, worker.Options{})
	defer worker_.Stop()

	worker_.RegisterWorkflow(workflows.HealthCheckWorkflow)
	worker_.RegisterWorkflow(workflows.IngresDocumentWorkflow)

	worker_.RegisterActivity(activities.CheckHealth)
	worker_.RegisterActivity(activities.ComputeFileHash)

	worker_.RegisterActivity(activities.S3GetPresignedDocumentURL)
	worker_.RegisterActivity(activities.S3PutDocument)
	worker_.RegisterActivity(activities.S3DocumentExists)

	worker_.RegisterActivity(activities.PGCreateDocument)

	worker_.RegisterActivity(activities.QdrantCreateCollection)
	worker_.RegisterActivity(activities.QdrantQueryCollection)
	worker_.RegisterActivity(activities.QdrantPutIntoCollection)

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

	router.GET("/health", func(c *gin.Context) {

		_, err := core.StartWorkflow(taskQueueName, workflows.HealthCheckWorkflow)
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
		fileData, err := core.GetFileData(file)
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": err})
		}

		// write to tmp
		tmpPath, err := core.WriteAsTemp(&fileData)
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": err})
		}

		// get bucket name
		bucketName := os.Getenv("DOCUMENT_S3_BUCKET")

		// run workflow
		ret, err := core.StartWorkflow(
			taskQueueName,
			workflows.IngresDocumentWorkflow,
			bucketName, // bucket_name
			tmpPath,
		)
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": "workflow error"})
			return
		}

		c.JSON(http.StatusOK, gin.H{"note": ret})
	})

	bind := os.Getenv("WORKFLOW_BIND")
	if bind == "" {
		log.Fatalln("'WORKFLOW_BIND' environment variable not set")
	}
	router.Run(bind)
}
