package src

import (
	"context"
	"fmt"
	"log"
	"time"

	"go.temporal.io/sdk/client"
	"go.temporal.io/sdk/contrib/envconfig"
)

func LoadTemporalConfigs(profile string) client.Options {

	// TODO get stage to insert into loading the config for devel, test, depl, etc.
	// STAGE := os.Getenv("STAGE")

	config, err := envconfig.LoadClientOptions(envconfig.LoadClientOptionsRequest{
		ConfigFilePath:    "./config.toml",
		ConfigFileProfile: profile,
	})
	if err != nil {
		log.Fatalln("Unable to find config file:", err)
	}
	return config
}

func StartWorkflow(task_queue_name string, workflow_ interface{}, args ...interface{}) (any, error) {

	client_, err := client.Dial(LoadTemporalConfigs("test"))
	if err != nil {
		log.Println("Unable to create client", err)
		return nil, err
	}
	defer client_.Close()

	// hard code workflow options
	workflowOptions := client.StartWorkflowOptions{
		ID:        fmt.Sprintf("%s-%d", funcName(workflow_), time.Now().UnixNano()),
		TaskQueue: task_queue_name,
	}

	// ctx := context.Background()
	// workflow.WithActivityOptions(ctx, workflow.ActivityOptions{})

	we, err := client_.ExecuteWorkflow(
		context.Background(),
		workflowOptions,
		workflow_,
		args...,
	)
	if err != nil {
		log.Println("Unable to execute Workflow:", err)
		return nil, err
	}
	log.Println("Started Workflow - WorkflowID:", we.GetID(), " - RunID:", we.GetRunID())

	var result any
	err = we.Get(context.Background(), &result)
	if err != nil {
		log.Println("Unable to get workflow result:", err)
		return nil, err
	}

	return result, nil
}
