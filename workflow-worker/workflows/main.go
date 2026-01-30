package workflows

import (
	"time"
	"workflow-worker/activities"

	"go.temporal.io/sdk/temporal"
	"go.temporal.io/sdk/workflow"
)

// define Rety Policy and Activity Options
type PolicyID int

const (
	FastRetry PolicyID = iota
	SafeRetry
)

var retryPolicies = map[PolicyID]temporal.RetryPolicy{
	FastRetry: {
		InitialInterval:    500 * time.Millisecond,
		BackoffCoefficient: 1.5,
		MaximumInterval:    10 * time.Second, // 100 * InitialInterval
		MaximumAttempts:    5,                // Unlimited
	},
	SafeRetry: {
		InitialInterval:    2 * time.Second,
		BackoffCoefficient: 2.0,
		MaximumInterval:    30 * time.Second, // 100 * InitialInterval
		MaximumAttempts:    3,                // Unlimited
	},
}

func activityOptionGenerator(id PolicyID) workflow.ActivityOptions {
	rp := retryPolicies[id]
	return workflow.ActivityOptions{
		StartToCloseTimeout: 60 * time.Second,
		RetryPolicy:         &rp,
	}
}

/*

WORKFLOWS TODO:

- SYSTEM INIT WORKFLOW
	- create buckets
	- migrate database
	- etc.

*/

// func VectorizeObjectFromS3(ctx workflow.Context, bucket_name string, obj_name string) (string, error) {
// 	ctx = workflow.WithActivityOptions(ctx, GENERAL_ACTIVITY_OPTIONS)

// 	// load object from
// 	var result []byte
// 	err := workflow.ExecuteActivity(ctx, LoadObjectFromS3, bucket_name, obj_name).Get(ctx, &result)
// 	if err != nil {
// 		return "", err
// 	}

// 	log.Println("Loaded image binary data:", result)

// 	return "", nil
// }

func IngresFileWorkflow(ctx workflow.Context, bucket_name string, file []byte) error {
	ctx = workflow.WithActivityOptions(ctx, activityOptionGenerator(SafeRetry))

	// generate file hash for ID
	var fileHash string
	err := workflow.ExecuteActivity(ctx, activities.ComputeFileHash, file).Get(ctx, &fileHash)
	if err != nil {
		return err
	}

	// put into S3
	err = workflow.ExecuteActivity(ctx, activities.S3_PUT, bucket_name, fileHash, file).Get(ctx, nil)
	if err != nil {
		return err
	}

	return nil
}

func HealthCheckWorkflow(ctx workflow.Context) error {
	ctx = workflow.WithActivityOptions(ctx, activityOptionGenerator(FastRetry))

	err := workflow.ExecuteActivity(ctx, activities.CheckHealth).Get(ctx, nil)
	if err != nil {
		return err
	}

	return nil
}

// func DocumentIngresWorkflow(ctx workflow.Context) {
// 	ctx = workflow.WithActivityOptions(ctx, workflow.ActivityOptions{
// 		// TaskQueue:           "document-ingress",
// 		StartToCloseTimeout: time.Second * 10,
// 	})

// 	// workflow.exe
// }
