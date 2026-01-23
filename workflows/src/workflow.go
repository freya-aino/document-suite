package src

import (
	"time"

	"go.temporal.io/sdk/workflow"
)

func OCRBucket(ctx workflow.Context, bucket_name string) ([]string, error) {
	ao := workflow.ActivityOptions{
		StartToCloseTimeout: time.Second * 10,
	}
	ctx = workflow.WithActivityOptions(ctx, ao)

	// get all documents in bucket
	var result []string
	err := workflow.ExecuteActivity(ctx, GetAllS3ObjectIDsInBucket, bucket_name).Get(ctx, &result)
	if err != nil {
		return []string{}, err
	}

	return result, nil
}

// func DocumentIngresWorkflow(ctx workflow.Context) {
// 	ctx = workflow.WithActivityOptions(ctx, workflow.ActivityOptions{
// 		// TaskQueue:           "document-ingress",
// 		StartToCloseTimeout: time.Second * 10,
// 	})

// 	// workflow.exe
// }
