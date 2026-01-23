package main

import (
	"context"
	"fmt"
	"log"
	"os"

	"go.temporal.io/sdk/client"
	"go.temporal.io/sdk/worker"

	src "workflows/src"
)

func StartWorker(task_queue_name string) {

	client, err := client.Dial(src.LoadTemporalConfigs("test"))
	if err != nil {
		log.Fatalln("Unable to create client", err)
	}
	defer client.Close()

	w := worker.New(client, task_queue_name, worker.Options{})

	w.RegisterWorkflow(src.OCRBucket)
	w.RegisterActivity(src.GetAllS3ObjectIDsInBucket)

	err = w.Run(worker.InterruptCh())
	if err != nil {
		log.Fatalln("Unable to start worker", err)
	}
}

func StartWorkflow_OCRBucket(task_queue_name string) {

	c, err := client.Dial(src.LoadTemporalConfigs("test"))
	if err != nil {
		log.Fatalln("Unable to create client", err)
	}
	defer c.Close()

	we, err := c.ExecuteWorkflow(
		context.Background(),
		client.StartWorkflowOptions{
			ID:        "OCR_BUCKET",
			TaskQueue: task_queue_name,
		},
		src.OCRBucket,
		"mlflow", // bucket name
	)

	if err != nil {
		log.Fatalln("Unable to execute Workflow", err)
	}
	log.Println("Started Workflow", "WorkflowID", we.GetID(), "RunID", we.GetRunID())

	var result []string
	err = we.Get(context.Background(), &result)
	if err != nil {
		log.Fatalln("Unable to get workflow result", err)
	}
	log.Println("Workflow result:", result)
}

func main() {

	if len(os.Args) <= 1 {
		log.Fatalln("Please provide an argument")
	}

	switch os.Args[1] {
	case "start-worker":
		StartWorker("default-task-queue")
	case "start-workflow":
		StartWorkflow_OCRBucket("default-task-queue")
	default:
		fmt.Println("no valid argument ('start-worker')")
	}

}
