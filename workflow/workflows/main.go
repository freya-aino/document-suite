package workflows

import (
	"time"
	"workflow/activities"

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

func IngresDocumentWorkflow(ctx workflow.Context, bucket_name string, tmpPath string) (string, error) {
	ctx = workflow.WithActivityOptions(ctx, activityOptionGenerator(SafeRetry))

	// generate file hash for ID
	var documentHash string
	err := workflow.ExecuteActivity(ctx, activities.ComputeFileHash, tmpPath).Get(ctx, &documentHash)
	if err != nil {
		return "", err
	}

	var documentExists bool
	err = workflow.ExecuteActivity(ctx, activities.S3DocumentExists, bucket_name, documentHash).Get(ctx, &documentExists)
	if err != nil {
		return "", err
	}

	// if no duplicate exist, put into S3
	if !documentExists {
		err = workflow.ExecuteActivity(ctx, activities.S3PutDocument, bucket_name, documentHash, tmpPath).Get(ctx, nil)
		if err != nil {
			return "", err
		}
		return "no-duplicate", nil
	}

	// get the file type

	// check if exists in postgres and create (with UUID) if not

	// seperate pages

	// for each page generate S3ID and UUID, then upload

	// update postgres with individual page references

	return "duplicate", nil
}

// func OCRWorkflow
// load all document IDs
// check OCR status and OCR version from PG
// filter non-ocr'ed
// fech all page IDs
// load and OCR each page
// check chuking status for document
// if not chunked with this chunking policy -> chuk Text
// save text chunks to PG
// update chuk status

// func VectorizeWorkflow
// load all document IDs
// check if OCR'ed and chunked -> else trigger OCRWorkflow
// load chuks associated with document and chunk policy
// check Vectorization status and version from PG for each
// filer unvectorized
// [unclear if this would just happen on setup, similar to the documents bucket] (check if qdrant collection exists -> create qdrant collection if not)
// vectorize each text chunk (TODO - add vectorization engine)
// store content and metadata in vectordb
// update vectorization status and version in PG

// func QueryVectorDBWorkflow (tool)

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
