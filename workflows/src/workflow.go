package src

import (
	"log"
	"time"

	"go.temporal.io/sdk/temporal"
	"go.temporal.io/sdk/workflow"
)

/*

WORKFLOWS TODO:

- SYSTEM INIT WORKFLOW
	- create buckets
	- migrate database
	- etc.

*/

var RETRY_POLICY = temporal.RetryPolicy{
	InitialInterval:    time.Second,
	BackoffCoefficient: 2.0,
	MaximumInterval:    time.Second * 10, // 100 * InitialInterval
	MaximumAttempts:    3,                // Unlimited
}

var GENERAL_ACTIVITY_OPTIONS = workflow.ActivityOptions{
	StartToCloseTimeout: time.Second * 10,
	RetryPolicy:         &RETRY_POLICY,
}

func OCRBucket(ctx workflow.Context, bucket_name string) ([]string, error) {
	ctx = workflow.WithActivityOptions(ctx, GENERAL_ACTIVITY_OPTIONS)

	// get all documents in bucket
	var result []string
	err := workflow.ExecuteActivity(ctx, GetAllS3ObjectIDsInBucket, bucket_name).Get(ctx, &result)
	if err != nil {
		return []string{}, err
	}

	return result, nil
}

func VectorizeObjectFromS3(ctx workflow.Context, bucket_name string, obj_name string) (string, error) {
	ctx = workflow.WithActivityOptions(ctx, GENERAL_ACTIVITY_OPTIONS)

	// load object from
	var result []byte
	err := workflow.ExecuteActivity(ctx, LoadObjectFromS3, bucket_name, obj_name).Get(ctx, &result)
	if err != nil {
		return "", err
	}

	log.Println("Loaded image binary data:", result)

	return "", nil
}

// func DocumentIngresWorkflow(ctx workflow.Context) {
// 	ctx = workflow.WithActivityOptions(ctx, workflow.ActivityOptions{
// 		// TaskQueue:           "document-ingress",
// 		StartToCloseTimeout: time.Second * 10,
// 	})

// 	// workflow.exe
// }
